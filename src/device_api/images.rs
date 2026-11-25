//! Rendered PNGs kept in memory for devices to download.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// Images kept per device.
const KEEP_PER_DEVICE: usize = 8;

struct Entry {
    filename: String,
    /// SHA-256 of the PNG, to find the entry when the same pixels come back.
    hash: String,
    png: Arc<Vec<u8>>,
}

/// The last few PNGs of every device, addressed by a filename that changes only
/// when the pixels change.
#[derive(Default)]
pub struct ImageStore {
    /// Newest last.
    devices: Mutex<HashMap<String, VecDeque<Entry>>>,
}

/// First `n` lowercase hex characters of the SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8], n: usize) -> String {
    Sha256::digest(bytes)
        .iter()
        .flat_map(|b| [b >> 4, b & 0xf])
        .take(n)
        .map(|nib| char::from_digit(u32::from(nib), 16).expect("nibble"))
        .collect()
}

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `png` for `device` and returns its filename (without `.png`):
    /// `{dashboard}-{6 hex of sha256(device)}-{unix secs when first stored}`.
    ///
    /// The firmware treats the first 13 characters (`<name>-<id>`) as the screen's
    /// identity: saving a new file deletes older files with the same start, and
    /// keeps others for up to 24 h. So that start never changes for a device's
    /// dashboard, and only the timestamp tells images apart. Storing the same
    /// pixels again returns the same filename.
    pub fn put(&self, device: &str, dashboard: &str, png: Vec<u8>, now: DateTime<Utc>) -> String {
        let hash = sha256_hex(&png, 64);
        let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        let images = devices.entry(device.to_owned()).or_default();
        if let Some(pos) = images.iter().position(|e| e.hash == hash) {
            let entry = images.remove(pos).expect("position is in range");
            let filename = entry.filename.clone();
            images.push_back(entry);
            return filename;
        }
        let prefix = format!("{dashboard}-{}-", sha256_hex(device.as_bytes(), 6));
        // Two different images in the same second still get different names.
        let mut secs = now.timestamp();
        while images
            .iter()
            .any(|e| e.filename == format!("{prefix}{secs}"))
        {
            secs += 1;
        }
        let filename = format!("{prefix}{secs}");
        images.push_back(Entry {
            filename: filename.clone(),
            hash,
            png: Arc::new(png),
        });
        while images.len() > KEEP_PER_DEVICE {
            images.pop_front();
        }
        filename
    }

    /// The PNG stored under `filename` (without `.png`), for any device.
    pub fn get(&self, filename: &str) -> Option<Vec<u8>> {
        let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        devices
            .values()
            .flat_map(|images| images.iter())
            .find(|e| e.filename == filename)
            .map(|e| e.png.as_ref().clone())
    }

    /// The filename most recently stored or re-served for `device`.
    pub fn latest(&self, device: &str) -> Option<String> {
        let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        devices
            .get(device)
            .and_then(|images| images.back())
            .map(|e| e.filename.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_795_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn same_pixels_same_filename() {
        let store = ImageStore::new();
        let a = store.put("kitchen", "family", vec![1, 2, 3], at(0));
        let b = store.put("kitchen", "family", vec![1, 2, 3], at(900));
        assert_eq!(a, b);
        assert_eq!(
            a,
            format!("family-{}-1795000000", sha256_hex(b"kitchen", 6))
        );
        assert!(a.len() <= 31, "{a}");
        let c = store.put("kitchen", "family", vec![4], at(1800));
        assert_ne!(a, c);
        assert!(c.ends_with("-1795001800"), "{c}");
        // The firmware's screen identity (first 13 chars) is stable across content.
        assert_eq!(a[..13], c[..13]);
        assert_eq!(store.get(&a), Some(vec![1, 2, 3]));
        assert_eq!(store.latest("kitchen"), Some(c));
    }

    #[test]
    fn same_second_different_pixels_differ() {
        let store = ImageStore::new();
        let a = store.put("kitchen", "family", vec![1], at(0));
        let b = store.put("kitchen", "family", vec![2], at(0));
        assert_ne!(a, b);
        assert_eq!(store.get(&a), Some(vec![1]));
        assert_eq!(store.get(&b), Some(vec![2]));
        assert!(b.ends_with("-1795000001"), "{b}");
    }

    #[test]
    fn devices_get_distinct_screen_ids() {
        let store = ImageStore::new();
        let a = store.put("kitchen", "family", vec![1], at(0));
        let b = store.put("hall", "family", vec![1], at(0));
        assert_ne!(a[..13], b[..13]);
    }

    #[test]
    fn homelab_filename_fits() {
        let f = ImageStore::new().put("office", "homelab", vec![0], at(0));
        assert!(f.len() <= 31, "{f}");
    }

    #[test]
    fn keeps_last_eight_per_device() {
        let store = ImageStore::new();
        let names: Vec<String> = (0..10u8)
            .map(|i| store.put("kitchen", "family", vec![i], at(i64::from(i))))
            .collect();
        let other = store.put("office", "homelab", vec![0], at(0));
        assert!(store.get(&names[0]).is_none());
        assert!(store.get(&names[1]).is_none());
        for n in &names[2..] {
            assert!(store.get(n).is_some(), "{n}");
        }
        assert!(store.get(&other).is_some());
        assert!(store.get("nope").is_none());
    }
}
