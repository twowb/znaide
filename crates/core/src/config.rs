use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 数据目录:默认 ~/.znaide,ZNAIDE_DATA_DIR 可覆盖(测试/多实例用)
pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("ZNAIDE_DATA_DIR") {
        return PathBuf::from(override_dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".znaide"))
        .unwrap_or_else(|| PathBuf::from(".znaide"))
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 默认模型,如 "qwen3:8b" / "deepseek-chat"
    pub model: Option<String>,
    /// OpenAI 兼容端点,如 http://localhost:11434/v1
    pub base_url: Option<String>,
    /// API key(本地端点可留空)
    pub api_key: Option<String>,
    /// 当前 provider(见 providers 表)
    pub provider: Option<String>,
    pub providers: std::collections::HashMap<String, ProviderDef>,
    /// 上下文窗口(token)。不设则查内置表,兜底 32k;
    /// ollama 要和运行时的 num_ctx 对上,否则占用条不准。
    pub context_window: Option<usize>,
    /// 全局人格(见 persona 模块);空 = 不注入。切换会写回这里持久生效。
    pub persona: Option<String>,
    /// 配置格式版本(内部键):保存时缺失自动补齐,供将来迁移判断。
    #[serde(default)]
    pub build_tag: Option<String>,
}

/// 当前配置格式版本(写入 config.json 的 build_tag;将来结构变化时据此迁移)
const CONFIG_TAG: &str = "a16416a02578";

/// provider 预设:端点 + 默认模型 + key(环境变量名或明文)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDef {
    pub base_url: Option<String>,
    /// 默认模型(没显式指定 model 时用它)
    pub model: Option<String>,
    /// 从该环境变量读 key,如 "DASHSCOPE_API_KEY"
    pub api_key_env: Option<String>,
    /// 明文 key(优先于 api_key_env)
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// 当前 provider 名(展示用)
    pub provider_name: String,
    /// 上下文窗口(None = 查内置表)
    pub context_window: Option<usize>,
}

/// 模型窗口内置表(没配 context_window 时按名字匹配)。
/// 数据 2026-09 从各家官方页 + Litellm 扒的,迭代很快,过时了就改;
/// 本地模型窗口看 ollama 的 num_ctx,对不上就在 config.json 写死。
fn model_context_window(model: &str) -> usize {
    let m = model.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains(k));
    // ---- 闭源/云端 ----
    if has(&["claude"]) {
        200_000 // opus/sonnet/haiku 4.x-5.x
    } else if has(&["gpt-5"]) {
        400_000 // gpt-5 / gpt-5-mini / gpt-5-nano
    } else if has(&["gpt-4.1"]) {
        1_048_576
    } else if has(&["o4-mini", "o4-mini", "o3", "o1"]) {
        200_000
    } else if has(&["gpt-4o"]) {
        128_000
    } else if has(&["gemini"]) || has(&["qwen-plus"]) {
        1_000_000 // gemini-2.5 系(官方站限区未能复核);qwen-plus(百炼 Qwen3 系)
    } else if has(&["qwen-max"]) {
        32_768 // 阿里百炼 qwen-max 官方页
    } else if has(&["deepseek-v4"]) {
        1_048_576 // DeepSeek v4(flash/pro)官方页 1M
    } else if has(&["deepseek-chat", "deepseek-reasoner"]) {
        131_072
    } else if has(&["glm-5"]) {
        1_048_576 // 智谱 GLM-5.3-flash 官方页 1M
    } else if has(&["glm-4", "glm4"]) {
        131_072
    } else if has(&["kimi-k3", "k3"]) {
        1_000_000 // 月之暗面 Kimi K3
    } else if has(&["kimi-k2", "moonshot-v1"]) {
        262_144 // Kimi K2 系列(2.6/2.7 官方 256k)
    } else if has(&["kimi"]) {
        131_072
    }
    // ---- 本地/开源(ollama 常用)----
    else if has(&["qwen3:4b", "qwen3:30b", "qwen3:235b"]) {
        262_144
    } else if has(&["qwen3"]) {
        40_960 // ollama qwen3:8b/14b/32b 等默认 40k
    } else if has(&["qwen2.5-coder", "qwen2.5", "qwen-coder"]) {
        32_768
    } else if has(&["llama3.3", "llama3.2", "llama3.1"]) {
        131_072 // ollama llama3.x 默认 128k
    } else if has(&["llama3"]) {
        8_192
    } else {
        32_768
    }
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok())
}

/// 内置 provider 预设;config.providers 可覆盖同名项
pub fn builtin_providers() -> std::collections::HashMap<String, ProviderDef> {
    let mut m = std::collections::HashMap::new();
    m.insert(
        "ollama".into(),
        ProviderDef {
            base_url: Some("http://localhost:11434/v1".into()),
            model: Some("qwen3:8b".into()),
            ..Default::default()
        },
    );
    m.insert(
        "dashscope".into(),
        ProviderDef {
            base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
            model: Some("qwen-plus".into()),
            api_key_env: Some("DASHSCOPE_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "deepseek".into(),
        ProviderDef {
            base_url: Some("https://api.deepseek.com/v1".into()),
            model: Some("deepseek-chat".into()),
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "openrouter".into(),
        ProviderDef {
            base_url: Some("https://openrouter.ai/api/v1".into()),
            model: Some("qwen/qwen3-8b".into()),
            api_key_env: Some("OPENROUTER_API_KEY".into()),
            ..Default::default()
        },
    );
    m.insert(
        "zai".into(),
        ProviderDef {
            base_url: Some("https://api.z.ai/api/v1".into()),
            model: Some("deepseek-ai/DeepSeek-R1-Distill-Qwen-32B".into()),
            api_key_env: Some("ZAI_API_KEY".into()),
            ..Default::default()
        },
    );
    m
}

impl Config {
    /// 合并后的 provider 表(内置 + 用户覆盖/新增)
    pub fn all_providers(&self) -> std::collections::HashMap<String, ProviderDef> {
        let mut m = builtin_providers();
        for (k, v) in &self.providers {
            m.insert(k.clone(), v.clone());
        }
        m
    }

    /// 读配置;文件不存在返回默认
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        let cfg: Config = serde_json::from_str(&text)?;
        Ok(cfg)
    }

    /// 算最终运行参数,来源优先级:CLI > 环境变量 > config > 内置预设
    pub fn resolve(
        &self,
        cli_model: Option<String>,
        cli_base_url: Option<String>,
        cli_api_key: Option<String>,
        cli_provider: Option<String>,
    ) -> anyhow::Result<Resolved> {
        let provider_name = cli_provider
            .or_else(|| env_first(&["ZNAIDE_PROVIDER"]))
            .or_else(|| self.provider.clone())
            .unwrap_or_else(|| "ollama".to_string());
        let providers = self.all_providers();
        let pdef = providers
            .get(&provider_name)
            .cloned()
            .unwrap_or_else(|| ProviderDef {
                base_url: Some(format!("https://{provider_name}/v1")),
                model: None,
                ..Default::default()
            });

        // base_url:CLI > env > 顶层 config > provider 预设
        let base_url = cli_base_url
            .or_else(|| env_first(&["ZNAIDE_BASE_URL", "OPENAI_BASE_URL"]))
            .or_else(|| self.base_url.clone())
            .or(pdef.base_url.clone())
            .unwrap_or_else(|| "http://localhost:11434/v1".to_string());

        // model:CLI > env > 顶层 config > 预设;再没有就报错
        let model = cli_model
            .or_else(|| env_first(&["ZNAIDE_MODEL", "OPENAI_MODEL"]))
            .or_else(|| self.model.clone())
            .or(pdef.model.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "未配置模型:请用 --model 指定,或设置 ZNAIDE_MODEL / config.json 的 model / provider「{provider_name}」的 model"
                )
            })?;

        // key:CLI > env > provider 明文 > provider 环境变量名 > config 顶层
        let api_key = cli_api_key
            .or_else(|| env_first(&["ZNAIDE_API_KEY", "OPENAI_API_KEY"]))
            .or_else(|| {
                pdef.api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        pdef.api_key_env.as_ref().and_then(|env| env_first(&[env]))
                    })
            })
            .or_else(|| self.api_key.clone().filter(|k| !k.is_empty()));

        Ok(Resolved {
            model,
            base_url,
            api_key,
            provider_name,
            context_window: self.context_window,
        })
    }

}

impl Resolved {
    /// 实际窗口:配置值 > 内置表 > 32k
    pub fn effective_context_window(&self) -> usize {
        match self.context_window {
            Some(n) if n > 0 => n,
            _ => model_context_window(&self.model),
        }
    }
}

impl Config {
    /// provider 清单(名字/base_url/模型),给 UI 列表用
    pub fn list_providers(&self) -> Vec<(String, String, String)> {
        let providers = self.all_providers();
        let mut out: Vec<(String, String, String)> = providers
            .into_iter()
            .map(|(name, def)| {
                let base = def
                    .base_url
                    .unwrap_or_else(|| "?".into());
                let model = def.model.unwrap_or_else(|| "(未设模型)".into());
                (name, base, model)
            })
            .collect();
        out.sort();
        out
    }

    /// 写盘前确保配置格式版本已打上(缺失则补当前版本)
    fn ensure_build_tag(&mut self) {
        if self.build_tag.is_none() {
            self.build_tag = Some(CONFIG_TAG.to_string());
        }
    }

    /// 把当前选择写进 config.json。表里没有的 provider 会补一份,方便以后手改。
    pub fn save(
        &mut self,
        provider: &str,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.provider = Some(provider.to_string());
        self.model = model.map(|s| s.to_string());
        self.base_url = base_url.map(|s| s.to_string());
        // api_key 为空字符串视为清除
        self.api_key = api_key
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        // 确保 provider 在表里(便于用户以后在文件里继续改)
        if !self.providers.contains_key(provider) {
            self.providers.insert(
                provider.to_string(),
                ProviderDef {
                    base_url: base_url.map(|s| s.to_string()),
                    model: model.map(|s| s.to_string()),
                    api_key: api_key.map(|s| s.to_string()),
                    api_key_env: None,
                },
            );
        }
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text + "\n")?;
        Ok(())
    }

    /// 新增/覆盖一个 provider 预设并落盘
    pub fn save_provider(&mut self, name: &str, def: &ProviderDef) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.providers.insert(name.to_string(), def.clone());
        self.persist()
    }

    /// 设置全局人格并落盘(空串 = 关闭人格)
    pub fn save_persona(&mut self, name: &str) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.persona = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
        self.persist()
    }

    /// 落盘(整份序列化写入)
    fn persist(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text + "\n")?;
        Ok(())
    }

    /// 配置格式标记需要迁移:build_tag 存在且不是当前版本(他版写入/外部改动)
    pub fn needs_migrate(&self) -> bool {
        matches!(&self.build_tag, Some(t) if t != CONFIG_TAG)
    }

    /// 执行迁移:把 build_tag 置回当前版本并落盘;返回是否发生过迁移
    pub fn migrate(&mut self) -> anyhow::Result<bool> {
        if !self.needs_migrate() {
            return Ok(false);
        }
        self.build_tag = Some(CONFIG_TAG.to_string());
        self.persist()?;
        Ok(true)
    }
}

/// 是否已存在配置文件
pub fn config_exists() -> bool {
    config_path().exists()
}

/// 建数据目录(memories/skills/sessions),不生成任何配置文件;
/// 首次创建返回 true(引导首启用用)
pub fn ensure_data_dirs() -> anyhow::Result<bool> {
    let first = !data_dir().exists();
    for sub in ["memories", "skills", "sessions"] {
        std::fs::create_dir_all(data_dir().join(sub))?;
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resolve_uses_ollama_preset() {
        // 空配置:默认 provider=ollama 自带 base_url+model
        let cfg = Config::default();
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.provider_name, "ollama");
        assert_eq!(r.base_url, "http://localhost:11434/v1");
        assert_eq!(r.model, "qwen3:8b");
    }

    #[test]
    fn cli_overrides_config() {
        let cfg = Config {
            model: Some("cfg-model".into()),
            base_url: Some("http://cfg".into()),
            api_key: None,
            provider: None,
            context_window: None,
            persona: None,
            build_tag: None,
            providers: Default::default(),
        };
        let r = cfg
            .resolve(Some("cli-model".into()), None, None, None)
            .unwrap();
        assert_eq!(r.model, "cli-model");
        assert_eq!(r.base_url, "http://cfg");
        assert_eq!(r.provider_name, "ollama");
    }

    #[test]
    fn provider_preset_applies() {
        let cfg = Config::default();
        // 指定 dashscope,无顶层 model → 用预设模型与 key env
        let r = cfg
            .resolve(None, None, None, Some("dashscope".into()))
            .unwrap();
        assert_eq!(r.base_url, "https://dashscope.aliyuncs.com/compatible-mode/v1");
        assert_eq!(r.model, "qwen-plus");
        // api_key 从 env 读不到时应为 None(不报错)
    }

    #[test]
    fn user_provider_overrides_builtin() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                base_url: Some("http://127.0.0.1:8080/v1".into()),
                model: Some("llama3".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            model: None,
            base_url: None,
            api_key: None,
            provider: Some("ollama".into()),
            context_window: None,
            persona: None,
            build_tag: None,
            providers,
        };
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(r.model, "llama3");
    }

    #[test]
    fn context_window_defaults_from_model_table() {
        // 配置优先
        let mut cfg = Config {
            model: Some("qwen3:8b".into()),
            context_window: Some(65536),
            ..Default::default()
        };
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), 65536);
        // 未配置 → 按模型名匹配(2026-09 检索值)
        cfg.context_window = None;
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), 40960); // ollama qwen3:8b
        // 云端/闭源家族
        assert_eq!(model_context_window("qwen-plus"), 1_000_000);
        assert_eq!(model_context_window("claude-sonnet-4-5"), 200_000);
        assert_eq!(model_context_window("gpt-5"), 400_000);
        assert_eq!(model_context_window("deepseek-v4-pro"), 1_048_576);
        // 未知模型兜底 32k
        assert_eq!(model_context_window("my-custom-model"), 32_768);
    }

    /// build_tag(配置格式版本):老配置无此键读为 None;写盘前由 save 系补上
    #[test]
    fn build_tag_roundtrip_and_legacy_default() {
        let old: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(old.build_tag, None, "老配置(无 build_tag)应读为 None");
        let with_tag: Config =
            serde_json::from_str(r#"{"build_tag":"a16416a02578"}"#).unwrap();
        assert_eq!(with_tag.build_tag.as_deref(), Some(CONFIG_TAG));
    }

    /// 迁移判定:当前版本/无标记不触发;异源标记触发(启动时自动置回)
    #[test]
    fn migrate_detects_foreign_build_tag() {
        let cur: Config = serde_json::from_str(r#"{"build_tag":"a16416a02578"}"#).unwrap();
        assert!(!cur.needs_migrate(), "当前版本标记不需迁移");
        assert!(!Config::default().needs_migrate(), "无标记(初次)不需迁移");
        let foreign: Config = serde_json::from_str(r#"{"build_tag":"deadbeef00"}"#).unwrap();
        assert!(foreign.needs_migrate(), "异源标记应触发迁移");
    }
}
