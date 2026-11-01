use crate::store::Entry;
use base64::Engine as _;
use reqwest::{blocking::Client, redirect::Policy};
use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;
fn error(value: &str) -> Box<EvalAltResult> {
    value.into()
}
fn dynamic(value: Value) -> ScriptResult<Dynamic> {
    rhai::serde::to_dynamic(value).map_err(|_| error("数据转换失败"))
}

pub fn engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("import").disable_symbol("eval");
    engine
        .set_max_operations(2_000_000)
        .set_max_call_levels(32)
        .set_max_string_size(16 * 1024 * 1024)
        .set_max_array_size(20_000)
        .set_max_map_size(256);
    engine.on_print(|_| {}).on_debug(|_, _, _| {});
    engine.register_fn("json_parse", |value: &str| -> ScriptResult<Dynamic> {
        dynamic(serde_json::from_str(value).map_err(|_| error("JSON 解析失败"))?)
    });
    engine.register_fn("json_stringify", |value: Dynamic| -> ScriptResult<String> {
        let value: Value = rhai::serde::from_dynamic(&value).map_err(|_| error("JSON 转换失败"))?;
        serde_json::to_string(&value).map_err(|_| error("JSON 转换失败"))
    });
    engine.register_fn("url_encode", |value: &str| -> String {
        url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
    });
    engine.register_fn("html_select",|html:&str,selector:&str|->ScriptResult<Dynamic>{
        let selector=scraper::Selector::parse(selector).map_err(|_|error("CSS 选择器无效"))?;
        let document=scraper::Html::parse_document(html);
        let items=document.select(&selector).take(20_000).map(|node|{
            let attrs=node.value().attrs().map(|(key,value)|(key.to_owned(),Value::String(value.to_owned()))).collect::<serde_json::Map<_,_>>();
            json!({"text":node.text().collect::<String>().trim(),"html":node.html(),"attrs":attrs})
        }).collect::<Vec<_>>();
        dynamic(json!(items))
    });
    engine.register_fn("rss_items",|xml:&str|->ScriptResult<Dynamic>{
        let document=roxmltree::Document::parse(xml).map_err(|_|error("RSS XML 解析失败"))?;
        let items=document.descendants().filter(|node|node.is_element()&&node.tag_name().name()=="item").take(20_000).map(|item|{
            let text=|name:&str|item.children().find(|n|n.is_element()&&n.tag_name().name().eq_ignore_ascii_case(name))
                .and_then(|n|n.text()).unwrap_or("").trim().to_string();
            let enclosure=item.children().find(|n|n.is_element()&&n.tag_name().name()=="enclosure");
            json!({"title":text("title"),"guid":text("guid"),"page_url":text("link"),
                "description":text("description"),"publish_time":text("pubDate"),
                "download_url":enclosure.and_then(|n|n.attribute("url")).unwrap_or(""),
                "size":enclosure.and_then(|n|n.attribute("length")).and_then(|v|v.parse::<i64>().ok()).unwrap_or(0)})
        }).collect::<Vec<_>>();
        dynamic(json!(items))
    });
    engine
}

#[derive(Clone)]
struct Http {
    client: Client,
    base: url::Url,
    site: Value,
    allowed: Vec<String>,
    credential_domains: Vec<String>,
    calls: Arc<AtomicUsize>,
    started: Instant,
}

impl Http {
    fn new(entry: &Entry, site: &Value) -> Result<Self, String> {
        let base = url::Url::parse(site["url"].as_str().ok_or("缺少站点 URL")?)
            .map_err(|_| "站点 URL 无效")?;
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .redirect(Policy::none());
        if site["use_proxy"].as_bool() == Some(true) {
            let proxy = std::env::var("MEDIARY_PLUGIN_PROXY_URL")
                .map_err(|_| "站点启用了代理，但主程序未配置代理")?;
            builder = builder.proxy(reqwest::Proxy::all(&proxy).map_err(|_| "站点代理无效")?);
        } else {
            builder = builder.no_proxy()
        }
        let client = builder.build().map_err(|_| "创建站点 HTTP 客户端失败")?;
        Ok(Self {
            client,
            base,
            site: site.clone(),
            allowed: entry
                .package
                .domains
                .iter()
                .chain(&entry.package.network_domains)
                .cloned()
                .collect(),
            credential_domains: entry.package.credential_domains.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            started: Instant::now(),
        })
    }

    fn url(&self, value: &str) -> ScriptResult<url::Url> {
        let url = self.base.join(value).map_err(|_| error("请求 URL 无效"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || !url
                .host_str()
                .is_some_and(|host| self.allowed.iter().any(|domain| domain == host))
        {
            return Err(error("请求地址不在适配包声明的域名范围内"));
        }
        if self.base.scheme() == "https" && url.scheme() == "http" {
            return Err(error("拒绝从 HTTPS 降级到 HTTP"));
        }
        Ok(url)
    }

    fn request(&self, options: Value, binary: bool) -> ScriptResult<Value> {
        if self.calls.fetch_add(1, Ordering::Relaxed) >= 32
            || self.started.elapsed() > Duration::from_secs(55)
        {
            return Err(error("超过单次适配的请求数量或时间限制"));
        }
        let mut target = self.url(
            options["url"]
                .as_str()
                .ok_or_else(|| error("请求缺少 url"))?,
        )?;
        let method = options["method"].as_str().unwrap_or("GET");
        if !matches!(method, "GET" | "POST" | "HEAD") {
            return Err(error("HTTP 方法仅支持 GET/POST/HEAD"));
        }
        let method = method
            .parse::<reqwest::Method>()
            .map_err(|_| error("HTTP 方法无效"))?;
        for redirect in 0..=3 {
            let mut request = self
                .client
                .request(method.clone(), target.clone())
                .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .header(
                    "Accept",
                    if binary {
                        "application/x-bittorrent"
                    } else {
                        "application/json, text/html, application/xml"
                    },
                )
                .header("X-Requested-With", "XMLHttpRequest");
            if let Some(headers) = options["headers"].as_object() {
                for (key, value) in headers {
                    if ["cookie", "authorization", "x-api-key", "host"]
                        .contains(&key.to_ascii_lowercase().as_str())
                    {
                        return Err(error("个人凭据由主程序注入，不允许脚本设置认证头"));
                    }
                    request = request.header(
                        key,
                        value.as_str().ok_or_else(|| error("请求头必须为字符串"))?,
                    );
                }
            }
            let credentials_allowed = target.origin() == self.base.origin()
                || (self.base.scheme() == "https"
                    && target.scheme() == "https"
                    && target.port_or_known_default() == Some(443)
                    && target.host_str().is_some_and(|host| {
                        self.credential_domains.iter().any(|domain| domain == host)
                    }));
            if credentials_allowed {
                for (field, header) in [
                    ("cookie", "Cookie"),
                    ("authorization", "Authorization"),
                    ("api_key", "x-api-key"),
                ] {
                    let auth = options["auth"].as_str().unwrap_or("all");
                    if auth == "none" || (auth != "all" && auth != field) {
                        continue;
                    }
                    if let Some(secret) = self.site[field].as_str().filter(|v| !v.trim().is_empty())
                    {
                        request = request.header(header, secret);
                    }
                }
            }
            if let Some(body) = options.get("body").filter(|v| !v.is_null()) {
                if let Some(body) = body.as_str() {
                    request = request.body(body.to_owned())
                } else {
                    request = request.json(body)
                }
            }
            let response = request
                .send()
                .map_err(|_| error("站点 HTTP 请求失败（连接、超时或 TLS 错误）"))?;
            let status = response.status();
            let charset = options["encoding"].as_str().map(str::to_owned).or_else(|| {
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| {
                        value
                            .split(';')
                            .find_map(|part| part.trim().strip_prefix("charset="))
                    })
                    .map(|value| value.trim_matches(['\"', '\'']).to_owned())
            });
            if status.is_redirection() {
                if redirect == 3 || method != reqwest::Method::GET {
                    return Err(error("站点重定向次数过多或 POST 需要显式处理"));
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| error("重定向缺少 Location"))?;
                let next = target
                    .join(location)
                    .map_err(|_| error("重定向 URL 无效"))?;
                target = self.url(next.as_str())?;
                continue;
            }
            let limit = if binary {
                10 * 1024 * 1024
            } else {
                16 * 1024 * 1024
            };
            let mut bytes = Vec::new();
            response
                .take(limit + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| error("读取站点响应失败"))?;
            if bytes.len() as u64 > limit {
                return Err(error("站点响应超过大小限制"));
            }
            if binary {
                if !status.is_success() {
                    return Err(error(&format!("下载返回 HTTP {}", status.as_u16())));
                }
                return Ok(json!(
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ));
            }
            let encoding = charset
                .as_deref()
                .map(|charset| {
                    encoding_rs::Encoding::for_label(charset.as_bytes())
                        .ok_or_else(|| error("站点响应字符编码不受支持"))
                })
                .transpose()?
                .unwrap_or(encoding_rs::UTF_8);
            let (body, _, _) = encoding.decode(&bytes);
            let body = body.into_owned();
            let json = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
            return Ok(json!({"status":status.as_u16(),"body":body,"json":json}));
        }
        Err(error("重定向失败"))
    }
}

pub fn execute(
    entry: &Entry,
    operation: &str,
    site: &Value,
    input: &Value,
) -> Result<Value, String> {
    let http = Http::new(entry, site)?;
    let started = Instant::now();
    let mut engine = engine();
    engine.on_progress(move |_| {
        (started.elapsed() > Duration::from_secs(60)).then(|| Dynamic::from("适配执行超时"))
    });
    let request = http.clone();
    engine.register_fn("http", move |options: Dynamic| -> ScriptResult<Dynamic> {
        let options: Value =
            rhai::serde::from_dynamic(&options).map_err(|_| error("请求选项无效"))?;
        dynamic(request.request(options, false)?)
    });
    let get = http.clone();
    engine.register_fn("http_get", move |url: &str| -> ScriptResult<Dynamic> {
        dynamic(get.request(json!({"url":url}), false)?)
    });
    let download = http.clone();
    engine.register_fn("torrent_get", move |url: &str| -> ScriptResult<String> {
        if operation_is_preview(&download.site) {
            return Err(error("预览不允许下载种子"));
        }
        Ok(download
            .request(json!({"url":url}), true)?
            .as_str()
            .unwrap_or_default()
            .to_owned())
    });
    let base = http.base.clone();
    engine.register_fn("absolute_url", move |value: &str| -> ScriptResult<String> {
        let url = base.join(value).map_err(|_| error("URL 无效"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(error("URL 必须为 HTTP(S)"));
        }
        Ok(url.to_string())
    });
    let context = json!({"site": {"id":site["id"],"name":site["name"],"url":site["url"],"rss_url":site["rss_url"]},
        "input":input,"settings":entry.settings});
    let context = rhai::serde::to_dynamic(context).map_err(|_| "构建脚本上下文失败")?;
    let ast = engine
        .compile(&entry.package.script)
        .map_err(|_| "适配脚本编译失败")?;
    let result: Dynamic = engine
        .call_fn(&mut Scope::new(), &ast, operation, (context,))
        .map_err(|e| {
            format!(
                "适配脚本执行失败，位置 {}；请核对接口与字段（异常内容已隐藏以保护凭据）",
                e.position()
            )
        })?;
    rhai::serde::from_dynamic(&result).map_err(|_| "适配脚本输出不能转换为 JSON".into())
}

fn operation_is_preview(site: &Value) -> bool {
    site["preview"].as_bool() == Some(true)
}
