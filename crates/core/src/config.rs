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
    /// 上下文窗口(token),按 provider 各配各的;不设则按模型名查内置表,
    /// **查不到就是"未知"**(不再兜底 32k,见 `effective_context_window`)。
    /// ollama 要和运行时的 num_ctx 对上,否则占用条不准。
    pub context_window: Option<usize>,
    /// 单条消息最多允许的模型往返轮数(一轮可含多次工具调用)。不设 = 200;
    /// 0 = 不限(只靠"重复调用同一工具"刹车)。命令行 `--max-turns` 优先于这里。
    pub max_turns: Option<usize>,
    /// 全局人格(见 persona 模块);空 = 不注入。切换会写回这里持久生效。
    pub persona: Option<String>,
    /// 配置格式版本(内部键):保存时缺失自动补齐,供将来迁移判断。
    #[serde(default)]
    pub build_tag: Option<String>,
}

/// 当前配置格式版本(写入 config.json 的 build_tag)。
/// v2:顶层 model/base_url/api_key/context_window 不再由面板写入,改为搬进对应
/// provider 条目(顶层只作手动临时覆盖),因此换一次标记触发一次性自愈迁移。
const CONFIG_TAG: &str = "c6ee35b45916";

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
    /// 该服务商对应模型的上下文窗口(可选)。多 provider 各配各的,
    /// 避免在 40k 的 ollama 与 1M 的云端模型之间切换时占用条算错。
    pub context_window: Option<usize>,
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
/// **查不到一律返回 None(未知)**:以前这里回 32k,会把百万级模型算成"快满了",
/// 占用条虚高还会劝人做没必要的 /compact。
fn model_context_window(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains(k));
    // ---- 闭源/云端 ----
    let win: usize = if has(&["claude"]) {
        200_000 // opus/sonnet/haiku 4.x-5.x
    } else if has(&["gpt-5"]) {
        400_000 // gpt-5 / gpt-5-mini / gpt-5-nano
    } else if has(&["gpt-4.1"]) {
        1_048_576
    } else if has(&["o4-mini", "o3", "o1"]) {
        200_000
    } else if has(&["gpt-4o"]) {
        128_000
    } else if has(&["gemini"]) || has(&["qwen-plus"]) {
        1_000_000 // gemini-2.5 系(官方站限区未能复核);qwen-plus(百炼 Qwen3 系)
    } else if has(&["qwen3-max", "qwen3.8-max"]) {
        1_000_000 // 百炼 Qwen3 代 max 档:与表内同代的 qwen-plus 对齐(确切值请写进条目覆盖)
    } else if has(&["qwen-max"]) {
        32_768 // 阿里百炼 qwen-max 官方页
    } else if has(&["deepseek-v4", "deepseek-flash", "deepseek-pro"]) {
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
        // 认不出来就认"不知道",不拿默认值假装知道
        return None;
    };
    Some(win)
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
        // 字段级合并:内置条目为底,用户条目只覆盖它写了值的字段。
        // (整条替换会让 `{"base_url": ...}` 顺手丢掉内置的 model/api_key_env)
        for (k, v) in &self.providers {
            match m.get_mut(k) {
                Some(base) => {
                    if v.base_url.is_some() {
                        base.base_url = v.base_url.clone();
                    }
                    if v.model.is_some() {
                        base.model = v.model.clone();
                    }
                    if v.api_key.is_some() {
                        base.api_key = v.api_key.clone();
                    }
                    if v.api_key_env.is_some() {
                        base.api_key_env = v.api_key_env.clone();
                    }
                    if v.context_window.is_some() {
                        base.context_window = v.context_window;
                    }
                }
                None => {
                    m.insert(k.clone(), v.clone());
                }
            }
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

        // key:CLI > env > 顶层 config(手动覆盖)> provider 明文 > provider 环境变量名。
        // 三个字段口径统一为"顶层覆盖预设",避免切 provider 时出现"新 key + 老端点"的错配。
        let api_key = cli_api_key
            .or_else(|| env_first(&["ZNAIDE_API_KEY", "OPENAI_API_KEY"]))
            .or_else(|| self.api_key.clone().filter(|k| !k.is_empty()))
            .or_else(|| pdef.api_key.clone().filter(|k| !k.is_empty()))
            .or_else(|| pdef.api_key_env.as_ref().and_then(|env| env_first(&[env])));

        Ok(Resolved {
            model,
            base_url,
            api_key,
            provider_name,
            // 窗口按 provider 走(各服务商各配各的);顶层那个是历史遗留兜底
            context_window: pdef.context_window.or(self.context_window),
        })
    }

}

impl Resolved {
    /// 实际窗口:配置值 > 内置表;**都查不到 = None(未知)**,调用方据此只报绝对量、
    /// 不编百分比(见 tui 的 ctx 占用条)。
    pub fn effective_context_window(&self) -> Option<usize> {
        match self.context_window {
            Some(n) if n > 0 => Some(n),
            _ => model_context_window(&self.model),
        }
    }
}

impl Config {
    /// 单条消息的轮数上限:配置值 > 默认 200;0 表示不限(不替换成默认值)
    pub fn effective_max_turns(&self) -> usize {
        self.max_turns.unwrap_or(crate::session::DEFAULT_MAX_TURNS)
    }

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

    /// 把当前选择写进 config.json:值写进**对应 provider 条目**(表里没有就补一份),
    /// 不再写顶层三件套——顶层只作手动临时覆盖,面板保存不该把预设永久遮蔽掉。
    /// model/base_url 传 None 表示"保持该条目原值";api_key 传空串表示清除明文。
    /// context_window 传 None 表示"该条目不固定窗口"(按模型名查内置表,查不到 = 未知)。
    pub fn save(
        &mut self,
        provider: &str,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        context_window: Option<usize>,
    ) -> anyhow::Result<()> {
        self.ensure_build_tag();
        self.provider = Some(provider.to_string());
        {
            let entry = self.providers.entry(provider.to_string()).or_default();
            if let Some(m) = model {
                entry.model = Some(m.to_string());
            }
            if let Some(b) = base_url {
                entry.base_url = Some(b.to_string());
            }
            if let Some(k) = api_key {
                // 空串视为清除明文(api_key_env 保留,环境变量那条路仍可用)
                entry.api_key = Some(k.to_string()).filter(|s| !s.is_empty());
            }
            // 0/None 都算"不固定"(0 不是有效窗口,当没填)
            entry.context_window = context_window.filter(|n| *n > 0);
        }
        // 顶层三件套清空:留着会遮蔽预设,让"切 provider"失效
        self.model = None;
        self.base_url = None;
        self.api_key = None;
        self.persist()
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

    /// 执行迁移:把 build_tag 置回当前版本,并把历史遗留的"顶层三件套 + 顶层窗口"
    /// 搬进当前 provider 条目(它们只是手动临时覆盖,长期留着会遮蔽预设、让切 provider 失效)。
    /// 返回是否发生过迁移。
    pub fn migrate(&mut self) -> anyhow::Result<bool> {
        if !self.needs_migrate() {
            return Ok(false);
        }
        self.build_tag = Some(CONFIG_TAG.to_string());
        self.absorb_top_level_into_provider();
        self.persist()?;
        Ok(true)
    }

    /// 把顶层 model/base_url/api_key/context_window 搬进当前 provider 条目并清空顶层。
    /// 口径统一后顶层三件套一律"手动覆盖"(优先于预设),所以"搬进条目 + 清空顶层"
    /// 之后 `resolve()` 的结果必然与搬之前一致(见 migrate_absorbs_top_level_overrides)。
    fn absorb_top_level_into_provider(&mut self) {
        if self.model.is_none()
            && self.base_url.is_none()
            && self.api_key.is_none()
            && self.context_window.is_none()
        {
            return;
        }
        let name = self
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "ollama".to_string());
        let entry = self.providers.entry(name).or_default();
        if let Some(m) = self.model.take() {
            entry.model = Some(m);
        }
        if let Some(b) = self.base_url.take() {
            entry.base_url = Some(b);
        }
        if let Some(cw) = self.context_window.take() {
            entry.context_window = Some(cw);
        }
        // 口径统一后顶层三件套一律"手动覆盖"(优先于预设),所以顶层 key 一定有生效,
        // 直接搬进条目即可——搬完生效值不变。
        if let Some(k) = self.api_key.take() {
            entry.api_key = Some(k);
        }
    }
}

/// 是否已存在配置文件
pub fn config_exists() -> bool {
    config_path().exists()
}

/// resolve() 会读取的环境变量里,有没有哪个已经设了非空值。
/// 宿主判断"用户是不是已经提供了配置"时用它,别再各自手写清单——手写那份漏了
/// `ZNAIDE_PROVIDER`(单给一个 provider 名就能靠内置预设跑起来,却会被判成"没配置")。
pub fn env_configured() -> bool {
    let names = [
        "ZNAIDE_PROVIDER",
        "ZNAIDE_MODEL",
        "OPENAI_MODEL",
        "ZNAIDE_BASE_URL",
        "OPENAI_BASE_URL",
        "ZNAIDE_API_KEY",
        "OPENAI_API_KEY",
    ];
    names
        .iter()
        .any(|n| std::env::var(n).map(|v| !v.trim().is_empty()).unwrap_or(false))
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
            max_turns: None,
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
            max_turns: None,
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
        assert_eq!(r.effective_context_window(), Some(65536));
        // 未配置 → 按模型名匹配(2026-09 检索值)
        cfg.context_window = None;
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), Some(40960)); // ollama qwen3:8b
        // 云端/闭源家族
        assert_eq!(model_context_window("qwen-plus"), Some(1_000_000));
        assert_eq!(model_context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(model_context_window("gpt-5"), Some(400_000));
        assert_eq!(model_context_window("deepseek-v4-pro"), Some(1_048_576));
        // v4 代短名(真实 API id 不带 v4 时的别名)
        assert_eq!(model_context_window("deepseek-flash"), Some(1_048_576));
        // 云端 Qwen3 代 max:不能被 ollama 的 qwen3 兜底(40k)吞掉
        assert_eq!(model_context_window("qwen3.8-max"), Some(1_000_000));
        assert_eq!(model_context_window("qwen3-max"), Some(1_000_000));
        assert_eq!(model_context_window("qwen3:8b"), Some(40_960));
    }

    /// B:认不出来的模型返回 None(未知),不再假装 32k——否则百万级模型会被算成快满了
    #[test]
    fn unknown_model_window_is_none_not_guess() {
        assert_eq!(model_context_window("my-custom-model"), None);
        assert_eq!(model_context_window("deepseek-flash-v9-unknown"), Some(1_048_576)); // 命中别名
        let cfg = Config {
            model: Some("my-custom-model".into()),
            ..Default::default()
        };
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.effective_context_window(), None, "查不到 = 未知,交给调用方只说绝对量");
        // 面板/配置里写死就照写死(0 无效,当没填)
        let cfg2 = Config {
            model: Some("my-custom-model".into()),
            context_window: Some(262_144),
            ..Default::default()
        };
        let r2 = cfg2.resolve(None, None, None, None).unwrap();
        assert_eq!(r2.effective_context_window(), Some(262_144));
    }

    #[test]
    fn effective_max_turns_defaults_and_overrides() {
        let mut cfg = Config::default();
        assert_eq!(cfg.effective_max_turns(), crate::session::DEFAULT_MAX_TURNS);
        cfg.max_turns = Some(500);
        assert_eq!(cfg.effective_max_turns(), 500);
        // 0 是"不限"的有效值,不能被替换成默认
        cfg.max_turns = Some(0);
        assert_eq!(cfg.effective_max_turns(), 0);
    }

    /// 轮数上限随 save() 落盘并读回——配置面板保存走的就是 `cfg.max_turns = …; save(…)`
    /// 这条路;顺带覆盖"老配置没有该键 → None"。
    #[test]
    fn max_turns_roundtrip_through_save() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_mt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        // 全新(无 config.json):未设置
        let mut cfg = Config::load().unwrap_or_default();
        assert_eq!(cfg.max_turns, None);

        // 面板保存:先设值再 save → 文件里能读回
        cfg.max_turns = Some(500);
        cfg.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(Config::load().unwrap().max_turns, Some(500));

        // 清空 → 写回 None(回到默认)
        let mut cfg = Config::load().unwrap();
        cfg.max_turns = None;
        cfg.save(
            "ollama",
            Some("qwen3:8b"),
            Some("http://127.0.0.1:11434/v1"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(Config::load().unwrap().max_turns, None);

        // 老配置没有该键照常读
        let legacy: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(legacy.max_turns, None);

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// build_tag(配置格式版本):老配置无此键读为 None;写盘前由 save 系补上
    #[test]
    fn build_tag_roundtrip_and_legacy_default() {
        let old: Config = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(old.build_tag, None, "老配置(无 build_tag)应读为 None");
        // 用常量拼,别再写死字面量(换版本时会一起失效)
        let with_tag: Config =
            serde_json::from_str(&format!(r#"{{"build_tag":"{CONFIG_TAG}"}}"#)).unwrap();
        assert_eq!(with_tag.build_tag.as_deref(), Some(CONFIG_TAG));
    }

    /// 迁移判定:当前版本/无标记不触发;旧版与异源标记都触发(启动时自动置回)
    #[test]
    fn migrate_detects_foreign_build_tag() {
        let cur: Config =
            serde_json::from_str(&format!(r#"{{"build_tag":"{CONFIG_TAG}"}}"#)).unwrap();
        assert!(!cur.needs_migrate(), "当前版本标记不需迁移");
        assert!(!Config::default().needs_migrate(), "无标记(初次)不需迁移");
        let v1: Config = serde_json::from_str(r#"{"build_tag":"a16416a02578"}"#).unwrap();
        assert!(v1.needs_migrate(), "上一版标记应触发一次性迁移");
        let foreign: Config = serde_json::from_str(r#"{"build_tag":"deadbeef00"}"#).unwrap();
        assert!(foreign.needs_migrate(), "异源标记应触发迁移");
    }

    /// D4:用户条目按字段覆盖内置预设,不能顺手丢掉它没写的字段
    #[test]
    fn user_preset_merges_field_wise() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "deepseek".into(),
            ProviderDef {
                base_url: Some("https://mirror.example.com/v1".into()),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            ..Default::default()
        };
        let all = cfg.all_providers();
        let d = all.get("deepseek").expect("内置 deepseek 应在");
        assert_eq!(d.base_url.as_deref(), Some("https://mirror.example.com/v1"), "用户写的覆盖");
        assert!(d.model.is_some(), "没写的 model 应保留内置值");
        assert!(d.api_key_env.is_some(), "没写的 api_key_env 应保留内置值");
        // 用户自定义的新 provider 照常加入
        let mut providers2 = std::collections::HashMap::new();
        providers2.insert(
            "myprov".into(),
            ProviderDef {
                base_url: Some("https://mine/v1".into()),
                model: Some("m".into()),
                ..Default::default()
            },
        );
        let cfg2 = Config {
            providers: providers2,
            ..Default::default()
        };
        assert!(cfg2.all_providers().contains_key("myprov"));
    }

    /// D3:上下文窗口按 provider 走,顶层那个只作兜底
    #[test]
    fn context_window_prefers_provider_entry() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "ollama".into(),
            ProviderDef {
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                model: Some("qwen3:8b".into()),
                context_window: Some(40_960),
                ..Default::default()
            },
        );
        let cfg = Config {
            providers,
            context_window: Some(9_999_999), // 历史遗留的全局值:不该盖过 provider 自己的
            ..Default::default()
        };
        let r = cfg.resolve(None, None, None, Some("ollama".into())).unwrap();
        assert_eq!(r.effective_context_window(), Some(40_960));
        // 没配 provider 窗口时,退回顶层,再退回模型表
        let cfg2 = Config {
            context_window: Some(65_536),
            ..Default::default()
        };
        let r2 = cfg2.resolve(None, None, None, Some("ollama".into())).unwrap();
        assert_eq!(r2.effective_context_window(), Some(65_536));
    }

    /// D1:面板保存写进 provider 条目,不再把顶层三件套写死(否则切 provider 失效)
    #[test]
    fn save_writes_into_provider_entry_not_top_level() {
        let _g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_cfg_save_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);

        let mut cfg = Config::default();
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            Some("sk-1"),
            Some(131_072),
        )
        .unwrap();

        assert_eq!(cfg.model, None, "顶层 model 不该被写");
        assert_eq!(cfg.base_url, None, "顶层 base_url 不该被写");
        assert_eq!(cfg.api_key, None, "顶层 api_key 不该被写");
        let d = cfg.providers.get("deepseek").expect("条目应写入");
        assert_eq!(d.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(d.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(d.api_key.as_deref(), Some("sk-1"));
        assert_eq!(d.context_window, Some(131_072), "窗口也写进条目(多服务商各配各的)");

        // 落盘后再读,值仍在条目里 → 切 provider 时不会被顶层遮蔽
        let back = Config::load().unwrap();
        assert!(back.model.is_none() && back.base_url.is_none());
        assert_eq!(
            back.all_providers().get("deepseek").and_then(|d| d.model.clone()),
            Some("deepseek-chat".to_string())
        );
        assert_eq!(
            back.all_providers().get("deepseek").and_then(|d| d.context_window),
            Some(131_072)
        );

        // 面板留空 = 该条目不固定窗口(清掉旧值,回到按模型名查表)
        let mut cfg = back;
        cfg.save(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com/v1"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.providers.get("deepseek").and_then(|d| d.context_window), None);
        assert_eq!(
            Config::load().unwrap().providers.get("deepseek").and_then(|d| d.context_window),
            None
        );

        std::env::remove_var("ZNAIDE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D5:自愈迁移把顶层三件套搬进当前 provider,且**迁移前后生效配置一致**
    #[test]
    fn migrate_absorbs_top_level_overrides() {
        const VAR: &str = "ZNAIDE_TEST_ABSORB_KEY";
        std::env::remove_var(VAR); // 分支 A:环境变量不存在
        let raw = format!(
            r#"{{
            "model": "deepseek-flash",
            "base_url": "https://api.deepseek.com/v1",
            "api_key": "sk-top",
            "provider": "deepseek",
            "context_window": 65536,
            "providers": {{
                "deepseek": {{"model": "deepseek-v4-flash", "api_key_env": "{VAR}"}},
                "ollama": {{"base_url": "http://127.0.0.1:11434/v1", "model": "qwen3:8b"}}
            }}
        }}"#
        );
        let before: Config = serde_json::from_str(&raw).unwrap();
        let eff_before = before.resolve(None, None, None, None).unwrap();

        let mut after: Config = serde_json::from_str(&raw).unwrap();
        after.build_tag = Some(CONFIG_TAG.to_string());
        after.absorb_top_level_into_provider();

        assert!(after.model.is_none() && after.base_url.is_none() && after.api_key.is_none());
        assert!(after.context_window.is_none(), "顶层窗口也该搬走");
        let d = after.providers.get("deepseek").unwrap();
        assert_eq!(d.model.as_deref(), Some("deepseek-flash"), "顶层值生效过 → 覆盖条目");
        assert_eq!(d.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(d.context_window, Some(65_536));
        assert_eq!(
            d.api_key.as_deref(),
            Some("sk-top"),
            "条目取不到 key 时,顶层明文要搬进来(否则用户丢 key)"
        );

        // 迁移前后生效参数必须一模一样
        let eff_after = after.resolve(None, None, None, None).unwrap();
        assert_eq!(eff_before.model, eff_after.model);
        assert_eq!(eff_before.base_url, eff_after.base_url);
        assert_eq!(eff_before.api_key, eff_after.api_key);
        assert_eq!(
            eff_before.effective_context_window(),
            eff_after.effective_context_window()
        );
        // 另一个 provider 不受影响,切过去仍好使(这正是本次修复的目的)
        let other = after.resolve(None, None, None, Some("ollama".into())).unwrap();
        assert_eq!(other.model, "qwen3:8b");
        assert_eq!(other.base_url, "http://127.0.0.1:11434/v1");

        // 分支 B:顶层与条目都有 key 时,顶层优先(口径已统一为"顶层 = 手动覆盖"),
        // 迁移把顶层值搬进条目 → 生效 key 前后一致
        let raw_b = r#"{
            "api_key": "sk-top",
            "provider": "ollama",
            "providers": { "ollama": {"model": "qwen3:8b", "api_key": "sk-old"} }
        }"#;
        let mut b: Config = serde_json::from_str(raw_b).unwrap();
        let eff_b = b.resolve(None, None, None, None).unwrap();
        assert_eq!(eff_b.api_key.as_deref(), Some("sk-top"), "顶层覆盖预设(三个字段口径一致)");
        b.absorb_top_level_into_provider();
        assert_eq!(b.providers.get("ollama").unwrap().api_key.as_deref(), Some("sk-top"));
        assert_eq!(
            b.resolve(None, None, None, None).unwrap().api_key.as_deref(),
            Some("sk-top"),
            "搬完生效 key 不变"
        );
    }
}
