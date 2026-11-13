//! Feature 注册表：插件内所有可插拔能力的统一入口。
//!
//! 加新功能只需两步：新建 `features/<name>.rs` 并在这里的 [`FEATURES`] 追加一行。
//! `lib.rs` 与 [`crate::registry`] 都不需要改动。

pub mod deepseek_search;
pub mod msys2_shell;
pub mod provider;

/// 一个可注册的插件能力。
pub struct Feature {
    /// 能力名，仅用于诊断输出（例如注册失败时定位是哪个能力）。
    pub name: &'static str,
    /// 注册函数：拿到宿主 API 表，返回 0 表示成功。
    pub register: fn(&rpi_plugin_sdk::PluginApi) -> i32,
}

/// 本插件启用的全部能力，按注册顺序执行。
///
/// 顺序有意义：同名 provider 由宿主按"先注册者胜"处理，因此顺序要与历史行为保持一致。
pub static FEATURES: &[Feature] = &[
    Feature {
        name: "provider",
        register: provider::register,
    },
    Feature {
        name: "msys2-bash",
        register: msys2_shell::register,
    },
    Feature {
        name: "deepseek-search",
        register: deepseek_search::register,
    },
];

#[cfg(test)]
mod tests {
    use super::FEATURES;

    #[test]
    fn feature_names_are_unique_and_non_empty() {
        let mut names: Vec<&str> = Vec::new();
        for feature in FEATURES {
            assert!(!feature.name.trim().is_empty(), "feature name must not be blank");
            assert!(
                !names.contains(&feature.name),
                "duplicate feature name: {}",
                feature.name
            );
            names.push(feature.name);
        }
    }

    #[test]
    fn provider_feature_is_registered_exactly_once() {
        // 重复注册同名 provider 会让宿主按"先注册者胜"丢弃后者，行为难以排查。
        assert_eq!(
            FEATURES
                .iter()
                .filter(|feature| feature.name == "provider")
                .count(),
            1
        );
    }
}
