//! Версия клиента устройства из заголовка `x-selfsync-client` (`plugin/0.3.0`):
//! кого затронет то, что старые версии не понимают (запись ключа формата 2, смена
//! ключа).

/// `X.Y.Z` → числа; суффикс после `-` или `+` отбрасывается.
fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// Версия из строки клиента (`plugin/0.3.0` → `0.3.0`).
pub fn client_version(client: &str) -> Option<(u32, u32, u32)> {
    let (_, v) = client.trim().split_once('/')?;
    parse(v.split_whitespace().next()?)
}

/// Клиент старее `min` (`"0.2.1"`). Пустая или неразборчивая строка — тоже «старее»:
/// так подключены клиенты до 0.3.0, которые заголовка не слали.
pub fn older_than(client: &str, min: &str) -> bool {
    match (client_version(client), parse(min)) {
        (Some(v), Some(m)) => v < m,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(!older_than("plugin/0.3.0", "0.2.1"));
        assert!(!older_than("cli/0.2.1", "0.2.1"));
        assert!(older_than("plugin/0.2.0", "0.2.1"));
        assert!(older_than("plugin/0.10.0", "0.11.0"));
        assert!(!older_than("plugin/0.10.0", "0.9.9"));
        assert!(!older_than("plugin/1.0.0-beta.1", "0.9.0"));
        // неизвестно — считается старым
        assert!(older_than("", "0.2.1"));
        assert!(older_than("plugin", "0.2.1"));
        assert!(older_than("plugin/x.y", "0.2.1"));
        assert!(older_than("plugin/1.2", "0.2.1"));
        assert_eq!(client_version(" cli/0.3.0 "), Some((0, 3, 0)));
    }
}
