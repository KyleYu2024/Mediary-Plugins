use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub format_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub domains: Vec<String>,
    #[serde(default)]
    pub network_domains: Vec<String>,
    #[serde(default)]
    pub credential_domains: Vec<String>,
    pub operations: Vec<String>,
    #[serde(default)]
    pub settings_fields: Vec<SettingField>,
    pub script: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub package: Package,
    pub enabled: bool,
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub previous: Option<Package>,
}

pub struct Store {
    pub entries: BTreeMap<String, Entry>,
    path: PathBuf,
    _lock: File,
}

fn private_file(path: &std::path::Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| "无法打开适配器数据文件".into())
}

impl Store {
    pub fn open() -> Result<Self, String> {
        let dir = PathBuf::from(
            std::env::var("MEDIARY_PLUGIN_DATA_DIR").map_err(|_| "缺少插件数据目录")?,
        );
        fs::create_dir_all(&dir).map_err(|_| "无法创建适配器数据目录")?;
        let lock = private_file(&dir.join("adapters.lock"))?;
        lock.lock_exclusive().map_err(|_| "无法锁定适配器配置")?;
        let path = dir.join("adapters.json");
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() > 32 * 1024 * 1024 {
                    return Err("适配器数据超过 32 MB".into());
                }
                serde_json::from_slice(&bytes).map_err(|_| "适配器数据损坏，已拒绝覆盖")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => return Err("读取适配器数据失败".into()),
        };
        Ok(Self {
            entries,
            path,
            _lock: lock,
        })
    }

    pub fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.entries).map_err(|_| "配置序列化失败")?;
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("适配器数据超过 32 MB".into());
        }
        let temp = self.path.with_extension("tmp");
        let mut file = private_file(&temp)?;
        file.set_len(0).map_err(|_| "清理临时文件失败")?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "保存适配器配置失败")?;
        fs::rename(temp, &self.path).map_err(|_| "替换适配器配置失败".into())
    }

    pub fn import(&mut self, package: Package, replace: bool) -> Result<(), String> {
        package.validate()?;
        if self.entries.len() >= 64 && !self.entries.contains_key(&package.id) {
            return Err("最多保存 64 份适配包".into());
        }
        if let Some(entry) = self.entries.get_mut(&package.id) {
            if !replace {
                return Err("已有同 ID 适配包，请确认替换".into());
            }
            entry.previous = Some(std::mem::replace(&mut entry.package, package));
            entry.enabled = false;
        } else {
            self.entries.insert(
                package.id.clone(),
                Entry {
                    package,
                    enabled: false,
                    settings: Map::new(),
                    previous: None,
                },
            );
        }
        self.save()
    }

    pub fn check_conflict(&self, id: &str) -> Result<(), String> {
        let entry = self.entries.get(id).ok_or("适配包不存在")?;
        for other in self
            .entries
            .values()
            .filter(|other| other.enabled && other.package.id != id)
        {
            if entry
                .package
                .domains
                .iter()
                .any(|domain| other.package.domains.contains(domain))
                && entry
                    .package
                    .operations
                    .iter()
                    .any(|op| other.package.operations.contains(op))
            {
                return Err(format!(
                    "与已启用的 {} 存在站点及操作冲突",
                    other.package.name
                ));
            }
        }
        Ok(())
    }
}

impl Package {
    pub fn validate(&self) -> Result<(), String> {
        if self.credential_domains.len() > 16
            || self.credential_domains.iter().any(|domain| {
                !self.network_domains.contains(domain) && !self.domains.contains(domain)
            })
        {
            return Err("凭据域名必须同时在站点或网络域名清单中声明".into());
        }
        if self.format_version != 1 {
            return Err("不支持的适配包格式版本".into());
        }
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b))
        {
            return Err("适配包 ID 只能包含小写字母、数字、下划线和连字符".into());
        }
        if self.name.trim().is_empty() || self.name.len() > 128 || self.description.len() > 4096 {
            return Err("适配包名称或说明无效".into());
        }
        semver::Version::parse(&self.version).map_err(|_| "适配版本必须符合 SemVer，例如 1.0.0")?;
        if self.domains.is_empty() || self.domains.len() > 16 || self.network_domains.len() > 16 {
            return Err("需要 1–16 个站点域名".into());
        }
        for domain in self.domains.iter().chain(&self.network_domains) {
            if domain.to_ascii_lowercase() != *domain
                || domain.contains(['/', ':', '@', '*', ' '])
                || !domain.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err("域名必须为精确的小写主机名，不支持通配符或 URL".into());
            }
        }
        if self.operations.is_empty()
            || self.operations.len() > 6
            || self.operations.iter().any(|op| {
                !matches!(
                    op.as_str(),
                    "latest" | "search" | "rss" | "download" | "profile" | "messages"
                )
            })
        {
            return Err("适配操作只能为 latest/search/rss/download/profile/messages".into());
        }
        if self
            .operations
            .iter()
            .any(|op| matches!(op.as_str(), "latest" | "search" | "rss"))
            && !self.operations.iter().any(|op| op == "download")
        {
            return Err("资源抓取适配必须同时提供 download 操作".into());
        }
        if self.script.len() > 512 * 1024 {
            return Err("脚本超过 512 KB".into());
        }
        let ast = crate::runtime::engine()
            .compile(&self.script)
            .map_err(|e| format!("脚本编译失败: {e}"))?;
        for op in &self.operations {
            if !ast
                .iter_functions()
                .any(|f| f.name == op && f.params.len() == 1)
            {
                return Err(format!("脚本缺少 fn {op}(ctx)"));
            }
        }
        let mut keys = std::collections::HashSet::new();
        if self.settings_fields.len() > 32
            || self
                .settings_fields
                .iter()
                .any(|f| f.key.is_empty() || f.key.len() > 64 || !keys.insert(&f.key))
        {
            return Err("本机配置字段无效或重复".into());
        }
        Ok(())
    }
}
