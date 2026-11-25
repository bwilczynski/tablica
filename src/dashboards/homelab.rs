//! Homelab dashboard view model (placeholder until the dashboard exists).

use serde::Serialize;

use crate::text::pl;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HomelabView {
    pub message: &'static str,
}

pub fn build() -> HomelabView {
    HomelabView {
        message: pl::HOMELAB_PLACEHOLDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_placeholder() {
        assert_eq!(build().message, "Homelab: wkrótce");
    }
}
