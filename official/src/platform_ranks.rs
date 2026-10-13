//! Anonymous HTTP readers for platform charts. These charts have different metrics.
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

pub const TENCENT_URL: &str = "https://pbaccess.video.qq.com/trpc.tencentvideohotlistdata.hotlistdata.DoSearchHotListHttp/getSearchHotListHttp?ChannelID=2&PageSize=50&PageNum=0&SceneID=2";
pub const IQIYI_URL: &str = "https://www.iqiyi.com/ranks1/2/0";
pub const MANGO_URL: &str = "https://mobileso.bz.mgtv.com/pc/suggest/v1?src=mgtv&pc=1&q=";

#[derive(Debug)]
pub struct RankItem {
    pub title: String,
    pub rank: usize,
    pub heat: Option<f64>,
    pub year: Option<i32>,
}

pub fn source(platform: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match platform {
        "tencent" => Some(("tencent-tv-search", "腾讯电视剧热搜榜", "腾讯视频")),
        "iqiyi" => Some(("iqiyi-tv-hot", "爱奇艺电视剧热播榜", "爱奇艺")),
        "mango" => Some(("mango-tv-popular", "芒果电视剧热门榜", "芒果TV")),
        _ => None,
    }
}

pub async fn fetch(client: &Client, platform: &str) -> Result<Vec<RankItem>, String> {
    let (url, referer) = match platform {
        "tencent" => (TENCENT_URL, "https://v.qq.com/"),
        "iqiyi" => (IQIYI_URL, "https://www.iqiyi.com/"),
        "mango" => (MANGO_URL, "https://www.mgtv.com/"),
        _ => return Err("该平台尚无已验证的公开榜单来源".into()),
    };
    // The shared HTTP client has no default authorization or cookie headers.
    let response = client
        .get(url)
        .header("Referer", referer)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    match platform {
        "iqiyi" => parse_iqiyi(&response.text().await.map_err(|e| e.to_string())?),
        "tencent" => parse_tencent(&response.json::<Value>().await.map_err(|e| e.to_string())?),
        "mango" => parse_mango(&response.json::<Value>().await.map_err(|e| e.to_string())?),
        _ => unreachable!(),
    }
}

fn title(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn validate_items(items: Vec<RankItem>, original_count: usize) -> Result<Vec<RankItem>, String> {
    if original_count > 0 && items.is_empty() {
        return Err("平台榜单条目缺少有效剧名，响应结构可能变化".into());
    }
    Ok(items)
}

pub fn parse_tencent(payload: &Value) -> Result<Vec<RankItem>, String> {
    let data = payload.get("data").unwrap_or(payload);
    if data["ErrCode"].as_i64() != Some(0) {
        return Err("腾讯榜单接口返回失败状态".into());
    }
    let channel = data["Results"]
        .as_array()
        .and_then(|channels| {
            channels
                .iter()
                .find(|c| c["ChannelID"].as_str() == Some("2"))
        })
        .ok_or("腾讯响应缺少电视剧榜单")?;
    let list = channel
        .pointer("/Result/ListInfo")
        .and_then(Value::as_array)
        .ok_or("腾讯电视剧榜单缺少有效列表")?;
    let items = list
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            // Do not consume a mixed general hot-search chart as TV subscriptions.
            if item
                .get("VideoType")
                .is_some_and(|kind| kind.as_str() != Some("2"))
            {
                return None;
            }
            Some(RankItem {
                title: title(item, "Query")?,
                rank: index + 1,
                heat: item["HotLevel"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0),
                year: None,
            })
        })
        .collect();
    validate_items(items, list.len())
}

pub fn parse_mango(payload: &Value) -> Result<Vec<RankItem>, String> {
    if payload["code"].as_i64() != Some(200) {
        return Err("芒果榜单接口返回失败状态".into());
    }
    let list = payload
        .pointer("/data/topList")
        .and_then(Value::as_array)
        .and_then(|groups| groups.iter().find(|g| g["label"] == "电视剧"))
        .and_then(|group| group["data"].as_array())
        .ok_or("芒果响应缺少电视剧热门榜")?;
    let items = list
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            Some(RankItem {
                title: title(item, "name")?,
                rank: item["index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(index + 1),
                heat: None,
                year: None,
            })
        })
        .collect();
    validate_items(items, list.len())
}

fn attribute(tag: &str, key: &str) -> Option<String> {
    let pattern = Regex::new(&format!(
        r#"(?i)(?:\s){}\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(key)
    ))
    .unwrap();
    let captures = pattern.captures(tag)?;
    let raw = captures.get(1).or_else(|| captures.get(2))?.as_str();
    quick_xml::escape::unescape(raw)
        .ok()
        .map(|s| s.into_owned())
}

pub fn parse_iqiyi(html: &str) -> Result<Vec<RankItem>, String> {
    if !html.contains("电视剧风云榜") {
        return Err("爱奇艺响应不是电视剧榜单页面，可能需要校验或页面已变化".into());
    }
    let cards = Regex::new(r"(?is)<a\b[^>]*>.*?</a\s*>").unwrap();
    let tags = Regex::new(r"(?is)<div\b[^>]*>").unwrap();
    let heat_pattern = Regex::new(r#"(?is)<span\b[^>]*class=["'][^"']*\brvi__index__num\b[^"']*["'][^>]*>\s*([0-9]+(?:\.[0-9]+)?)\s*</span>"#).unwrap();
    let mut items = Vec::new();
    let mut card_count = 0;
    for card in cards.find_iter(html) {
        let open = card.as_str().split('>').next().unwrap_or_default();
        if !attribute(open, "class").is_some_and(|s| s.split_whitespace().any(|c| c == "rvi__box"))
        {
            continue;
        }
        card_count += 1;
        let mut name = None;
        let mut year = None;
        for tag in tags.find_iter(card.as_str()) {
            let class = attribute(tag.as_str(), "class").unwrap_or_default();
            if class.split_whitespace().any(|c| c == "rvi__tit1") {
                name = attribute(tag.as_str(), "title").filter(|s| !s.trim().is_empty());
            }
            if class.split_whitespace().any(|c| c == "rvi__type1") {
                year = attribute(tag.as_str(), "title")
                    .and_then(|s| s.split('/').next()?.trim().parse::<i32>().ok())
                    .filter(|y| (1900..=2100).contains(y));
            }
        }
        if let Some(name) = name {
            let rank = attribute(open, "data-rseat-v2")
                .and_then(|s| s.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(card_count);
            let heat = heat_pattern
                .captures(card.as_str())
                .and_then(|c| c[1].parse::<f64>().ok());
            items.push(RankItem {
                title: name.trim().to_string(),
                rank,
                heat,
                year,
            });
        }
    }
    if card_count == 0 {
        return Err("爱奇艺页面缺少电视剧排名条目，页面结构可能变化".into());
    }
    validate_items(items, card_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tencent_selects_tv_and_preserves_original_rank() {
        let data = json!({"data":{"ErrCode":0,"Results":[
            {"ChannelID":"0","Result":{"ListInfo":[{"Query":"综艺"}]}},
            {"ChannelID":"2","Result":{"ListInfo":[{"Query":"","VideoType":"2"},
                {"Query":"电视剧","VideoType":"2","HotLevel":123}, {"Query":"动漫","VideoType":"3"}]}}
        ]}});
        let items = parse_tencent(&data).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rank, 2);
        assert_eq!(items[0].heat, Some(123.0));
        assert!(parse_tencent(&json!({"data":{"ErrCode":1}})).is_err());
    }

    #[test]
    fn mango_selects_tv_without_inventing_heat_values() {
        let items = parse_mango(&json!({"code":200,"data":{"topList":[
            {"label":"综艺","data":[{"name":"综艺"}]},
            {"label":"电视剧","data":[{"name":"剧集","index":3}]}
        ]}}))
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rank, 3);
        assert_eq!(items[0].heat, None);
        assert!(parse_mango(&json!({"code":200,"data":{"topList":[]}})).is_err());
    }

    #[test]
    fn iqiyi_reads_rank_cards_and_ignores_other_page_titles() {
        let html = r#"<title>电视剧风云榜</title><div title='广告' class='rvi__tit1'></div>
            <a class='rvi__box rvi__box3' data-rseat-v2='4'><div class='rvi__tit1' title='剧名 &amp; 名字'></div>
            <div title='2026 / 都市' class='rvi__type1'></div><span class='rvi__index__num'>6987</span></a>"#;
        let items = parse_iqiyi(html).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "剧名 & 名字");
        assert_eq!(items[0].rank, 4);
        assert_eq!(items[0].year, Some(2026));
        assert_eq!(items[0].heat, Some(6987.0));
        assert!(parse_iqiyi("<html>请完成验证</html>").is_err());
        assert!(parse_iqiyi("<title>电视剧风云榜</title>").is_err());
    }
}
