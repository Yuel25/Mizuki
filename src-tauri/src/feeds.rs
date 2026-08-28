use crate::models::RssItem;
use rss::Channel;

fn nested_pub_dates(xml: &str) -> Vec<Option<String>> {
    xml.split("<item")
        .skip(1)
        .map(|item| {
            let end = item.find("</item>").unwrap_or(item.len());
            let item = &item[..end];
            let marker = item.find("pubDate>")?;
            let opening = item[..marker].rfind('<')?;
            let tag = &item[opening + 1..marker + 7];
            let value_start = marker + 8;
            let closing = format!("</{tag}>");
            let value_end = item[value_start..].find(&closing)? + value_start;
            Some(item[value_start..value_end].trim().to_owned())
        })
        .collect()
}

pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<(String, Vec<RssItem>), String> {
    let parsed = url::Url::parse(url).map_err(|_| "RSS 地址无效".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("RSS 地址必须使用 HTTP 或 HTTPS".into());
    }
    let bytes = client
        .get(parsed)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    let xml = String::from_utf8_lossy(&bytes);
    let nested_dates = nested_pub_dates(&xml);
    let channel = Channel::read_from(&bytes[..]).map_err(|e| format!("无法解析 RSS: {e}"))?;
    let items = channel
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let link = item.link().unwrap_or_default().to_owned();
            let enclosure = item
                .enclosure()
                .map(|entry| entry.url().to_owned())
                .filter(|url| {
                    url.starts_with("magnet:")
                        || url.starts_with("http://")
                        || url.starts_with("https://")
                });
            let extension = item
                .extensions()
                .get("torrent")
                .and_then(|group| group.get("link"))
                .and_then(|values| values.first())
                .and_then(|entry| entry.value.clone());
            let torrent = enclosure.or(extension);
            RssItem {
                guid: item
                    .guid()
                    .map(|guid| guid.value().to_owned())
                    .unwrap_or_else(|| link.clone()),
                feed_id: String::new(),
                title: item.title().unwrap_or("未命名资源").to_owned(),
                link,
                torrent,
                published_at: item
                    .pub_date()
                    .map(str::to_owned)
                    .or_else(|| nested_dates.get(index).cloned().flatten()),
                downloaded: false,
                download: None,
                matches_rule: false,
            }
        })
        .collect();
    Ok((channel.title().to_owned(), items))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_mikan_nested_pub_date() {
        let xml = "<rss><channel><item><title>A</title><torrent xmlns=\"x\"><pubDate>Fri, 28 Aug 2026 12:00:00 GMT</pubDate></torrent></item></channel></rss>";
        assert_eq!(
            nested_pub_dates(xml),
            vec![Some("Fri, 28 Aug 2026 12:00:00 GMT".into())]
        )
    }
}
