//! 插件入口装配：按顺序执行 [`crate::features::FEATURES`] 里的每个能力。
//!
//! 这一层不包含任何业务逻辑，只负责"谁来注册、失败怎么办"。

/// 依次注册所有 feature。
///
/// 参数：`api` 为宿主的插件 API 表。
/// 返回：`0` 表示全部成功；第一个失败 feature 的返回码（宿主据此报告
/// "register returned nonzero code"）。
///
/// 失败即中止：后续 feature 不再执行，避免在半注册状态下继续装载。
pub fn register_all(api: &rpi_plugin_sdk::PluginApi) -> i32 {
    for feature in crate::features::FEATURES {
        let rc = (feature.register)(api);
        if rc != 0 {
            eprintln!(
                "rpi-opencode-extras: feature `{}` failed with rc={rc}",
                feature.name
            );
            return rc;
        }
    }
    0
}
