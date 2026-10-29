// 配置读写：保持和 Node 版完全相同的 config.json 字段名，方便两个版本互换。
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Config {
    pub conversation_name: String,
    pub conversation_url: String,
    pub page_label: String,
    pub out_dir: String,
    pub max_files: u64,
    pub max_mb: u64,
    pub keep_days: u64,
    pub direct_mode: bool,
    pub ego_path: String,
    /// 启动时是否需要输密码（面板里可以关掉）
    pub require_password: bool,
    /// 无水印原图（可选）：开启后接口返回 image_ori_raw 档（无水印）。
    /// 默认关：接口返回 image_ori 档，同网页「下载原图」（全分辨率，带豆包AI生成水印）。
    pub raw_image: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            conversation_name: "生图专用".into(),
            conversation_url: String::new(),
            page_label: "p2".into(),
            out_dir: String::new(),
            max_files: 200,
            max_mb: 2048,
            keep_days: 0,
            direct_mode: false,
            ego_path: String::new(),
            require_password: true,
            raw_image: false,
        }
    }
}

impl Config {
    pub fn from_value(v: &Value) -> Config {
        let d = Config::default();
        let s = |k: &str, def: &str| -> String {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(|x| x.to_string())
                .unwrap_or_else(|| def.to_string())
        };
        let n = |k: &str, def: u64| -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(def) };
        Config {
            conversation_name: s("conversationName", &d.conversation_name),
            conversation_url: s("conversationUrl", &d.conversation_url),
            page_label: s("pageLabel", &d.page_label),
            out_dir: s("outDir", &d.out_dir),
            max_files: n("maxFiles", d.max_files),
            max_mb: n("maxMB", d.max_mb),
            keep_days: n("keepDays", d.keep_days),
            direct_mode: v.get("directMode").and_then(|x| x.as_bool()).unwrap_or(d.direct_mode),
            ego_path: s("egoPath", &d.ego_path),
            require_password: v
                .get("requirePassword")
                .and_then(|x| x.as_bool())
                .unwrap_or(d.require_password),
            raw_image: v.get("rawImage").and_then(|x| x.as_bool()).unwrap_or(d.raw_image),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "conversationName": self.conversation_name,
            "conversationUrl": self.conversation_url,
            "pageLabel": self.page_label,
            "outDir": self.out_dir,
            "maxFiles": self.max_files,
            "maxMB": self.max_mb,
            "keepDays": self.keep_days,
            "directMode": self.direct_mode,
            "egoPath": self.ego_path,
            "requirePassword": self.require_password,
            "rawImage": self.raw_image,
        })
    }

    pub fn load(path: &Path) -> Config {
        match fs::read_to_string(path) {
            Ok(t) => match serde_json::from_str::<Value>(&t) {
                Ok(v) => Config::from_value(&v),
                Err(_) => Config::default(),
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) {
        let body = serde_json::to_string_pretty(&self.to_value()).unwrap_or_else(|_| "{}".into());
        let _ = fs::write(path, body);
    }
}
