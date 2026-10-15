//! Settings written locally over USB by the web flasher, never by the publisher.
use alloc::string::String;
use serde::{Deserialize, Serialize};

pub const FLASH_OFFSET: u32 = 0x3f0000;
pub const SECTOR_SIZE: usize = 4096;
pub const MAX_JSON: usize = 2048;
pub const MAGIC: &[u8; 8] = b"UBUSCFG1";

// Deliberately no Debug: settings contain the user's passwords.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub wifi_ssid: String,
    pub wifi_password: String,
    pub ubus_url: String,
    pub ubus_username: String,
    pub ubus_password: String,
    pub wan_interface: String,
    pub wifi_device: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Missing,
    Corrupt,
    Invalid(&'static str),
}

impl DeviceConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bounded = |s: &str, min, max| s.len() >= min && s.len() <= max && !s.contains('\0');
        if !bounded(&self.wifi_ssid, 1, 32) {
            return Err(ConfigError::Invalid("Wi-Fi name"));
        }
        let password = &self.wifi_password;
        if !(password.is_empty()
            || bounded(password, 8, 63)
            || password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ConfigError::Invalid("Wi-Fi password"));
        }
        let authority = self
            .ubus_url
            .strip_prefix("http://")
            .and_then(|s| s.split('/').next());
        if !bounded(&self.ubus_url, 8, 256)
            || self
                .ubus_url
                .bytes()
                .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
            || authority.is_none_or(|a| a.is_empty() || a.contains(['@', '?', '#']))
            || self.ubus_url.contains('#')
        {
            return Err(ConfigError::Invalid("Router URL"));
        }
        if !bounded(&self.ubus_username, 1, 64) || !bounded(&self.ubus_password, 1, 128) {
            return Err(ConfigError::Invalid("Router login"));
        }
        let interface = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if !bounded(&self.wan_interface, 1, 64)
            || !interface(&self.wan_interface)
            || !bounded(&self.wifi_device, 0, 64)
            || !interface(&self.wifi_device)
        {
            return Err(ConfigError::Invalid("Network interface"));
        }
        Ok(())
    }
}

/// CRC-32/ISO-HDLC, also implemented by the browser config writer.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

pub fn decode_sector(bytes: &[u8]) -> Result<DeviceConfig, ConfigError> {
    if bytes.get(..8) != Some(MAGIC.as_slice()) {
        return Err(ConfigError::Missing);
    }
    let header = bytes.get(..16).ok_or(ConfigError::Corrupt)?;
    let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_JSON {
        return Err(ConfigError::Corrupt);
    }
    let payload = bytes.get(16..16 + len).ok_or(ConfigError::Corrupt)?;
    let checksum = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if crc32(payload) != checksum {
        return Err(ConfigError::Corrupt);
    }
    let config: DeviceConfig = serde_json::from_slice(payload).map_err(|_| ConfigError::Corrupt)?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> DeviceConfig {
        DeviceConfig {
            wifi_ssid: "test network".into(),
            wifi_password: "test-password".into(),
            ubus_url: "http://192.168.8.1:8080/ubus".into(),
            ubus_username: "dashboard".into(),
            ubus_password: "test-router-password".into(),
            wan_interface: "wan".into(),
            wifi_device: "".into(),
        }
    }
    #[test]
    fn configuration_survives_a_flash_sector_round_trip() {
        let config = settings();
        let json = serde_json::to_vec(&config).unwrap();
        let mut sector = [0xff; SECTOR_SIZE];
        sector[..8].copy_from_slice(MAGIC);
        sector[8..12].copy_from_slice(&(json.len() as u32).to_le_bytes());
        sector[12..16].copy_from_slice(&crc32(&json).to_le_bytes());
        sector[16..16 + json.len()].copy_from_slice(&json);
        assert_eq!(decode_sector(&sector).unwrap().wifi_ssid, config.wifi_ssid);
        sector[20] ^= 1;
        assert!(matches!(decode_sector(&sector), Err(ConfigError::Corrupt)));
        assert!(matches!(
            decode_sector(&[0xff; SECTOR_SIZE]),
            Err(ConfigError::Missing)
        ));
        assert!(matches!(decode_sector(MAGIC), Err(ConfigError::Corrupt)));
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
    }
    #[test]
    fn validates_byte_limits_router_urls_and_interface_names() {
        let mut config = settings();
        for url in [
            "https://router/ubus",
            "http:///ubus",
            "http://user:pass@router/ubus",
            "http://router/ubus\r\n",
        ] {
            config.ubus_url = url.into();
            assert!(config.validate().is_err());
        }
        config = settings();
        config.wan_interface = "wan/../../etc".into();
        assert!(config.validate().is_err());
        config = settings();
        config.wifi_ssid = "界".repeat(11);
        assert!(config.validate().is_err());
        config = settings();
        config.wifi_password = "".into();
        assert!(config.validate().is_ok());
        config.wifi_password = "x".into();
        assert!(config.validate().is_err());
    }
}
