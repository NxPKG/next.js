use std::{collections::HashMap, sync::Arc};

use anyhow::Context as _;
use napi::bindgen_prelude::*;
use next_page_static_info::{
    build_ast_from_source, collect_exports, collect_rsc_module_info, extract_expored_const_values,
    Const, ExportInfo, RscModuleInfo,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use turbopack_binding::swc::core::{
    base::{config::ParseOptions, try_with_handler},
    common::{
        comments::Comments, errors::ColorConfig, FileName, FilePathMapping, SourceMap, GLOBALS,
    },
};

use crate::util::MapErr;

pub struct ParseTask {
    pub filename: FileName,
    pub src: String,
    pub options: Buffer,
}

#[napi]
impl Task for ParseTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        GLOBALS.set(&Default::default(), || {
            let c = turbopack_binding::swc::core::base::Compiler::new(Arc::new(SourceMap::new(
                FilePathMapping::empty(),
            )));

            let options: ParseOptions = serde_json::from_slice(self.options.as_ref())?;
            let comments = c.comments().clone();
            let comments: Option<&dyn Comments> = if options.comments {
                Some(&comments)
            } else {
                None
            };
            let fm =
                c.cm.new_source_file(self.filename.clone(), self.src.clone());
            let program = try_with_handler(
                c.cm.clone(),
                turbopack_binding::swc::core::base::HandlerOpts {
                    color: ColorConfig::Never,
                    skip_filename: false,
                },
                |handler| {
                    c.parse_js(
                        fm,
                        handler,
                        options.target,
                        options.syntax,
                        options.is_module,
                        comments,
                    )
                },
            )
            .convert_err()?;

            let ast_json = serde_json::to_string(&program)
                .context("failed to serialize Program")
                .convert_err()?;

            Ok(ast_json)
        })
    }

    fn resolve(&mut self, _env: Env, result: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(result)
    }
}

#[napi]
pub fn parse(
    src: String,
    options: Buffer,
    filename: Option<String>,
    signal: Option<AbortSignal>,
) -> AsyncTask<ParseTask> {
    let filename = if let Some(value) = filename {
        FileName::Real(value.into())
    } else {
        FileName::Anon
    };
    AsyncTask::with_optional_signal(
        ParseTask {
            filename,
            src,
            options,
        },
        signal,
    )
}

/// wrap read file to suppress errors conditionally.
/// [NOTE] currently next.js passes _every_ file in the paths regardless of if
/// it's an asset or an ecmascript, So skipping non-utf8 read errors. Probably
/// should skip based on file extension.
fn read_file_wrapped_err(path: &str, raise_err: bool) -> Result<String> {
    let buf = std::fs::read(path).map_err(|e| {
        napi::Error::new(
            Status::GenericFailure,
            format!("Next.js ERROR: Failed to read file {}:\n{:#?}", path, e),
        )
    });

    match buf {
        Ok(buf) => Ok(String::from_utf8(buf).ok().unwrap_or("".to_string())),
        Err(e) if raise_err => Err(e),
        _ => Ok("".to_string()),
    }
}

/// A regex pattern to determine if is_dynamic_metadata_route should continue to
/// parse the page or short circuit and return false.
static DYNAMIC_METADATA_ROUTE_SHORT_CURCUIT: Lazy<Regex> =
    Lazy::new(|| Regex::new("generateImageMetadata|generateSitemaps").unwrap());

/// A regex pattern to determine if get_page_static_info should continue to
/// parse the page or short circuit and return default.
static PAGE_STATIC_INFO_SHORT_CURCUIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        "runtime|preferredRegion|getStaticProps|getServerSideProps|generateStaticParams|export \
         const",
    )
    .unwrap()
});

pub struct DetectMetadataRouteTask {
    page_file_path: String,
}

#[napi]
impl Task for DetectMetadataRouteTask {
    type Output = Option<ExportInfo>;
    type JsValue = Object;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        let file_content = read_file_wrapped_err(self.page_file_path.as_str(), true)?;

        if !DYNAMIC_METADATA_ROUTE_SHORT_CURCUIT.is_match(file_content.as_str()) {
            return Ok(None);
        }

        let (source_ast, _) = build_ast_from_source(&file_content, &self.page_file_path)?;
        collect_exports(&source_ast).map(Some).convert_err()
    }

    fn resolve(&mut self, env: Env, exports_info: Self::Output) -> napi::Result<Self::JsValue> {
        let mut ret = env.create_object()?;

        let mut warnings = env.create_array(0)?;

        match exports_info {
            Some(exports_info) => {
                let is_dynamic_metadata_route =
                    !exports_info.generate_image_metadata.unwrap_or_default()
                        || !exports_info.generate_sitemaps.unwrap_or_default();
                ret.set_named_property(
                    "isDynamicMetadataRoute",
                    env.get_boolean(is_dynamic_metadata_route),
                )?;

                for (key, message) in exports_info.warnings {
                    let mut warning_obj = env.create_object()?;
                    warning_obj.set_named_property("key", env.create_string(&key)?)?;
                    warning_obj.set_named_property("message", env.create_string(&message)?)?;
                    warnings.insert(warning_obj)?;
                }
                ret.set_named_property("warnings", warnings)?;
            }
            None => {
                ret.set_named_property("warnings", warnings)?;
                ret.set_named_property("isDynamicMetadataRoute", env.get_boolean(false))?;
            }
        }

        Ok(ret)
    }
}

/// Detect if metadata routes is a dynamic route, which containing
/// generateImageMetadata or generateSitemaps as export
#[napi]
pub fn is_dynamic_metadata_route(page_file_path: String) -> AsyncTask<DetectMetadataRouteTask> {
    AsyncTask::new(DetectMetadataRouteTask { page_file_path })
}

#[napi(object, object_to_js = false)]
pub struct CollectPageStaticInfoOption {
    pub page_file_path: String,
    pub is_dev: Option<bool>,
    pub page: Option<String>,
    pub page_type: String, //'pages' | 'app' | 'root'
}

pub struct CollectPageStaticInfoTask {
    option: CollectPageStaticInfoOption,
}

#[napi]
impl Task for CollectPageStaticInfoTask {
    type Output = Option<(ExportInfo, HashMap<String, Value>, RscModuleInfo, bool)>;
    type JsValue = Option<String>;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        let CollectPageStaticInfoOption {
            page_file_path,
            is_dev,
            ..
        } = &self.option;
        let file_content =
            read_file_wrapped_err(page_file_path.as_str(), !is_dev.unwrap_or_default())?;

        if !PAGE_STATIC_INFO_SHORT_CURCUIT.is_match(file_content.as_str()) {
            return Ok(None);
        }

        let (source_ast, comments) = build_ast_from_source(&file_content, page_file_path)?;
        let exports_info = collect_exports(&source_ast)?;
        let rsc_info = collect_rsc_module_info(&comments, true);

        let mut properties_to_extract = exports_info.extra_properties.clone();
        properties_to_extract.insert("config".to_string());

        let mut exported_const_values =
            extract_expored_const_values(&source_ast, properties_to_extract);

        let should_warn = exported_const_values
            .iter()
            .any(|(_, v)| matches!(v, Some(Const::Unsupported)));

        let mut extracted_values = HashMap::new();

        for (key, value) in exported_const_values.drain() {
            if let Some(Const::Value(v)) = value {
                extracted_values.insert(key.clone(), v);
            }
        }

        Ok(Some((
            exports_info,
            extracted_values,
            rsc_info,
            should_warn,
        )))
    }

    fn resolve(&mut self, _env: Env, result: Self::Output) -> napi::Result<Self::JsValue> {
        if let Some((exports_info, extracted_values, rsc_info, should_warn)) = result {
            // [TODO] this is stopgap; there are some non n-api serializable types in the
            // nested result. However, this is still much smaller than passing whole ast.
            // Should go away once all of logics in the getPageStaticInfo is internalized.
            let ret = StaticPageInfo {
                exports_info: Some(exports_info),
                extracted_values,
                rsc_info: Some(rsc_info),
                should_warn,
            };

            let ret = serde_json::to_string(&ret)
                .context("failed to serialize static info result")
                .convert_err()?;

            Ok(Some(ret))
        } else {
            Ok(None)
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticPageInfo {
    pub exports_info: Option<ExportInfo>,
    pub extracted_values: HashMap<String, Value>,
    pub rsc_info: Option<RscModuleInfo>,
    pub should_warn: bool,
}

#[napi]
pub fn get_page_static_info(
    option: CollectPageStaticInfoOption,
) -> AsyncTask<CollectPageStaticInfoTask> {
    AsyncTask::new(CollectPageStaticInfoTask { option })
}
