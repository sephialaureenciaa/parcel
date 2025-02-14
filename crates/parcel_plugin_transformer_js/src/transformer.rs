use anyhow::{anyhow, Error};

use parcel_core::plugin::TransformerPlugin;
use parcel_core::plugin::{RunTransformContext, TransformResult, TransformationInput};
use parcel_core::types::engines::EnvironmentFeature;
use parcel_core::types::{Asset, BuildMode, FileType, LogLevel, OutputFormat, SourceType};

mod conversion;
#[cfg(test)]
mod test_helpers;

/// This is a rust only `TransformerPlugin` implementation for JS assets that goes through the
/// default SWC transformer.
///
/// The transformer is part of the `AssetRequest` and is responsible for:
///
/// * Parsing a JS/TS file
/// * Transforming the file using SWC
/// * Analyzing all its `require`/`import`/`export` statements and returning lists of found
///  `Dependency` as well as exported, imported and re-exported symbols (as `Symbol`, usually
///   mapping to a mangled name that the SWC transformer replaced in the source file + the source
///   module and the source name that has been imported)
#[derive(Debug)]
pub struct ParcelJsTransformerPlugin {}

impl ParcelJsTransformerPlugin {
  pub fn new() -> Self {
    Self {}
  }
}

impl TransformerPlugin for ParcelJsTransformerPlugin {
  /// This does a lot of equivalent work to `JSTransformer::transform` in
  /// `packages/transformers/js`
  fn transform(
    &mut self,
    context: &mut RunTransformContext,
    input: TransformationInput,
  ) -> Result<TransformResult, Error> {
    let env = input.env();
    let file_system = context.file_system();
    let is_node = env.context.is_node();
    let source_code = input.read_code(file_system)?;

    let transformation_result = parcel_js_swc_core::transform(
      parcel_js_swc_core::Config {
        code: source_code.bytes().to_vec(),
        // TODO Lift context up into constructor to improve performance?
        env: context
          .options()
          .env
          .clone()
          .unwrap_or_default()
          .iter()
          .map(|(key, value)| (key.as_str().into(), value.as_str().into()))
          .collect(),
        filename: input
          .file_path()
          .to_str()
          .ok_or_else(|| anyhow!("Invalid non UTF-8 file-path"))?
          .to_string(),
        insert_node_globals: !is_node && env.source_type != SourceType::Script,
        is_browser: env.context.is_browser(),
        is_development: context.options().mode == BuildMode::Development,
        is_esm_output: env.output_format == OutputFormat::EsModule,
        is_library: env.is_library,
        is_worker: env.context.is_worker(),
        node_replacer: is_node,
        project_root: context.project_root().to_string_lossy().into_owned(),
        replace_env: !is_node,
        scope_hoist: env.should_scope_hoist && env.source_type != SourceType::Script,
        source_maps: env.source_map.is_some(),
        source_type: match env.source_type {
          SourceType::Module => parcel_js_swc_core::SourceType::Module,
          SourceType::Script => parcel_js_swc_core::SourceType::Script,
        },
        supports_module_workers: env.should_scope_hoist
          && env.engines.supports(EnvironmentFeature::WorkerModule),
        trace_bailouts: context.options().log_level == LogLevel::Verbose,
        ..parcel_js_swc_core::Config::default()
      },
      None,
    )?;

    // TODO handle errors properly
    if let Some(errors) = transformation_result.diagnostics {
      return Err(anyhow!(format!("{:#?}", errors)));
    }

    let file_path = input.file_path();
    let asset_type = FileType::from_extension(
      file_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default(),
    );

    let asset = Asset {
      asset_type,
      code: source_code,
      env: env.clone(),
      file_path: file_path.to_path_buf(),
      ..Asset::default()
    };

    let config = parcel_js_swc_core::Config::default();
    let options = context.options();
    let result = conversion::convert_result(asset, &config, transformation_result, &options)
      // TODO handle errors properly
      .map_err(|_err| anyhow!("Failed to transform"))?;

    Ok(result)
  }
}

#[cfg(test)]
mod test {
  use std::path::PathBuf;
  use std::sync::Arc;

  use parcel_core::plugin::{
    RunTransformContext, TransformResult, TransformationInput, TransformerPlugin,
  };
  use parcel_core::types::{
    Asset, Code, Dependency, FileType, Location, ParcelOptions, SourceLocation, SpecifierType,
    Symbol,
  };
  use parcel_filesystem::in_memory_file_system::InMemoryFileSystem;

  use crate::ParcelJsTransformerPlugin;

  fn empty_asset() -> Asset {
    Asset {
      asset_type: FileType::Js,
      ..Default::default()
    }
  }

  #[test]
  fn test_asset_id_is_stable() {
    let source_code = Arc::new(Code::from(String::from("function hello() {}")));

    let asset_1 = Asset {
      code: source_code.clone(),
      file_path: "mock_path".into(),
      ..Asset::default()
    };

    let asset_2 = Asset {
      code: source_code,
      file_path: "mock_path".into(),
      ..Asset::default()
    };

    // This nº should not change across runs / compilation
    assert_eq!(asset_1.id(), 5787511958692361102);
    assert_eq!(asset_1.id(), asset_2.id());
  }

  #[test]
  fn test_transformer_on_noop_asset() {
    let source_code = Arc::new(Code::from(String::from("function hello() {}")));
    let target_asset = Asset {
      code: source_code,
      file_path: "mock_path.js".into(),
      ..Asset::default()
    };
    let asset_id = target_asset.id();
    let result = run_test(target_asset).unwrap();

    assert_eq!(
      result,
      TransformResult {
        asset: Asset {
          file_path: "mock_path.js".into(),
          asset_type: FileType::Js,
          // SWC inserts a newline here
          code: Arc::new(Code::from(String::from("function hello() {}\n"))),
          symbols: vec![],
          has_symbols: true,
          unique_key: Some(format!("{:016x}", asset_id)),
          ..empty_asset()
        },
        dependencies: vec![],
        invalidate_on_file_change: vec![]
      }
    );
  }

  #[test]
  fn test_transformer_on_asset_that_requires_other() {
    let source_code = Arc::new(Code::from(String::from(
      r#"
const x = require('other');
exports.hello = function() {};
    "#,
    )));
    let target_asset = Asset {
      code: source_code,
      file_path: "mock_path.js".into(),
      ..Asset::default()
    };
    let asset_id = target_asset.id();
    let result = run_test(target_asset).unwrap();

    let mut expected_dependencies = vec![Dependency {
      loc: Some(SourceLocation {
        file_path: PathBuf::from("mock_path.js"),
        start: Location {
          line: 2,
          column: 19,
        },
        end: Location {
          line: 2,
          column: 26,
        },
      }),
      placeholder: Some("e83f3db3d6f57ea6".to_string()),
      source_asset_id: Some(format!("{:016x}", asset_id)),
      source_path: Some(PathBuf::from("mock_path.js")),
      specifier: String::from("other"),
      specifier_type: SpecifierType::CommonJS,
      symbols: vec![Symbol {
        exported: String::from("*"),
        loc: None,
        local: String::from("$other$"),
        ..Symbol::default()
      }],
      ..Default::default()
    }];
    expected_dependencies[0].set_placeholder("e83f3db3d6f57ea6");
    expected_dependencies[0].set_kind("Require");

    assert_eq!(result.dependencies, expected_dependencies);
    assert_eq!(
      result,
      TransformResult {
        asset: Asset {
          file_path: "mock_path.js".into(),
          asset_type: FileType::Js,
          // SWC inserts a newline here
          code: Arc::new(Code::from(String::from(
            "const x = require(\"e83f3db3d6f57ea6\");\nexports.hello = function() {};\n"
          ))),
          symbols: vec![
            Symbol {
              exported: String::from("hello"),
              loc: Some(SourceLocation {
                file_path: PathBuf::from("mock_path.js"),
                start: Location { line: 3, column: 9 },
                end: Location {
                  line: 3,
                  column: 14
                }
              }),
              local: String::from("$hello"),
              ..Default::default()
            },
            Symbol {
              exported: String::from("*"),
              loc: Some(SourceLocation {
                file_path: PathBuf::from("mock_path.js"),
                start: Location { line: 1, column: 1 },
                end: Location { line: 1, column: 1 }
              }),
              local: String::from("$_"),
              ..Default::default()
            },
            Symbol {
              exported: String::from("*"),
              loc: None,
              local: format!("${:016x}$exports", asset_id),
              ..Default::default()
            }
          ],
          has_symbols: true,
          unique_key: Some(format!("{:016x}", asset_id)),
          ..empty_asset()
        },
        dependencies: expected_dependencies,
        invalidate_on_file_change: vec![]
      }
    );
  }

  fn run_test(asset: Asset) -> anyhow::Result<TransformResult> {
    let file_system = Arc::new(InMemoryFileSystem::default());
    let options = Arc::new(ParcelOptions::default());
    let mut context = RunTransformContext::new(file_system, options, PathBuf::default());
    let mut transformer = ParcelJsTransformerPlugin::new();
    let input = TransformationInput::Asset(asset);

    let result = transformer.transform(&mut context, input)?;
    Ok(result)
  }
}
