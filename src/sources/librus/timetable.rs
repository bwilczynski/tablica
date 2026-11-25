//! Librus timetable page parsing.

use scraper::{Html, Selector};

/// The `<select name="tydzien">` options of a timetable page, in page order:
/// `(value, selected)`. Values look like `YYYY-MM-DD_YYYY-MM-DD`.
pub fn week_options(html: &str) -> Vec<(String, bool)> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"select[name="tydzien"] option"#).expect("valid selector");
    doc.select(&sel)
        .filter_map(|opt| {
            let value = opt.value().attr("value")?;
            Some((value.to_owned(), opt.value().attr("selected").is_some()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><body><form method="post">
<select name="tydzien" onchange="this.form.submit();">
  <option value="2026-11-16_2026-11-22">16.11.2026 - 22.11.2026</option>
  <option value="2026-11-23_2026-11-29" selected="selected">23.11.2026 - 29.11.2026</option>
  <option value="2026-11-30_2026-12-06">30.11.2026 - 06.12.2026</option>
</select>
<select name="inny"><option value="x" selected>x</option></select>
</form></body></html>"#;

    #[test]
    fn week_options_finds_selected_and_next() {
        let opts = week_options(PAGE);
        assert_eq!(opts.len(), 3);
        let sel = opts.iter().position(|(_, s)| *s).unwrap();
        assert_eq!(opts[sel].0, "2026-11-23_2026-11-29");
        assert_eq!(opts[sel + 1], ("2026-11-30_2026-12-06".to_owned(), false));
        assert_eq!(opts.iter().filter(|(_, s)| *s).count(), 1);
    }

    #[test]
    fn week_options_empty_without_select() {
        assert!(week_options("<html><body>login</body></html>").is_empty());
    }
}
