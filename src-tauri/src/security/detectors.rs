//! v1.1 P1-4: 敏感数据自动检测（Sensitive data auto-detection）。
//!
//! 在 memory 内容被吸收到 sponge 引擎之前对其进行扫描。
//! 如果发现敏感模式，内容会被脱敏并发出警告。
//!
//! 检测器注册表（Detector Registry）：
//! 使用正则表达式检测以下敏感数据类型：
//! - API 密钥（API keys）
//! - Bearer 令牌（Bearer tokens）
//! - 私钥（Private keys）
//! - 中国居民身份证（China Resident ID）
//! - 中国手机号（China Mobile phone numbers）

use once_cell::sync::Lazy;
use parking_lot::RwLock;
use regex::Regex;
use std::sync::Arc;

/// 基于正则表达式的单类敏感数据检测器。
#[derive(Debug, Clone)]
pub struct SensitiveDetector {
    /// 检测器名称（如 "api_key", "china_id"）
    pub name: &'static str,
    /// 正则表达式模式
    pattern: Regex,
    /// 脱敏替换文本
    replacement: &'static str,
}

impl SensitiveDetector {
    /// 创建新的检测器。
    /// # Panic
    /// 如果正则表达式无效，则 panic（仅在开发时发生）。
    pub fn new(name: &'static str, pattern: &str, replacement: &'static str) -> Self {
        Self {
            name,
            pattern: Regex::new(pattern).expect("valid regex in detectors"),
            replacement,
        }
    }

    /// 扫描内容，返回（是否发现敏感数据, 脱敏后的内容）。
    pub fn scan(&self, content: &str) -> (bool, String) {
        let found = self.pattern.is_match(content);
        let redacted = self
            .pattern
            .replace_all(content, self.replacement)
            .to_string();
        (found, redacted)
    }
}

/// 预编译的中国身份证正则（18位，最后一位可能是X）
#[allow(dead_code)]
static CHINA_ID_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{17}[\dXx]\b").unwrap());

/// 预编译的中国手机号正则（11位，以1开头）
#[allow(dead_code)]
static CHINA_PHONE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b1[3-9]\d{9}\b").unwrap());

/// 检测器注册表 — 扫描内容对所有注册模式进行检测。
pub struct SensitiveScanner {
    detectors: Vec<SensitiveDetector>,
}

impl Default for SensitiveScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SensitiveScanner {
    /// 创建新的扫描器，注册所有内置检测器。
    pub fn new() -> Self {
        Self {
            detectors: vec![
                // API 密钥：常见前缀 + 20+ 字符
                SensitiveDetector::new(
                    "api_key",
                    r#"(?i)(api[_-]?key|apikey|secret[_-]?key|access[_-]?token)\s*[:=]\s*['"]?([A-Za-z0-9_\-]{20,})['"]?"#,
                    "$1: [REDACTED]",
                ),
                // Bearer 令牌
                SensitiveDetector::new(
                    "bearer_token",
                    r"(?i)bearer\s+([A-Za-z0-9_\-\.]{20,})",
                    "bearer [REDACTED]",
                ),
                // 私钥（PEM 格式）
                SensitiveDetector::new(
                    "private_key",
                    r"-----BEGIN\s+(RSA\s+)?PRIVATE\s+KEY-----",
                    "[REDACTED PRIVATE KEY]",
                ),
                // 中国居民身份证（18位）
                SensitiveDetector::new("china_id", r"\b\d{17}[\dXx]\b", "[REDACTED ID]"),
                // 中国手机号（11位，以1开头）
                SensitiveDetector::new("china_phone", r"\b1[3-9]\d{9}\b", "[REDACTED PHONE]"),
            ],
        }
    }

    /// 扫描内容对所有检测器进行检测。
    /// 返回（脱敏后的内容, 检测到的类别列表）。
    pub fn scan(&self, content: &str) -> (String, Vec<&'static str>) {
        let mut redacted = content.to_string();
        let mut categories = Vec::new();
        for detector in &self.detectors {
            let (found, result) = detector.scan(&redacted);
            if found {
                categories.push(detector.name);
                redacted = result;
            }
        }
        (redacted, categories)
    }
}

/// 全局共享的敏感数据扫描器实例。
/// 使用 `RwLock` 支持并发读访问。
static GLOBAL_SCANNER: Lazy<Arc<RwLock<SensitiveScanner>>> =
    Lazy::new(|| Arc::new(RwLock::new(SensitiveScanner::new())));

/// 对给定内容进行敏感数据扫描。
/// 返回（脱敏后的内容, 检测到的类别列表）。
pub fn scan_content(content: &str) -> (String, Vec<&'static str>) {
    GLOBAL_SCANNER.read().scan(content)
}

/// 检查内容是否包含敏感数据（仅检查，不脱敏）。
pub fn contains_sensitive(content: &str) -> bool {
    let (_, categories) = GLOBAL_SCANNER.read().scan(content);
    !categories.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_api_key() {
        let content = "API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx";
        let (redacted, categories) = scan_content(content);
        assert!(categories.contains(&"api_key"));
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains("sk-abc"));
    }

    #[test]
    fn detects_china_id() {
        let content = "身份证号：110101199003074518";
        let (redacted, categories) = scan_content(content);
        assert!(categories.contains(&"china_id"));
        assert!(redacted.contains("[REDACTED ID]"));
        assert!(!redacted.contains("110101199003074518"));
    }

    #[test]
    fn detects_china_phone() {
        let content = "联系电话：13812345678";
        let (redacted, categories) = scan_content(content);
        assert!(categories.contains(&"china_phone"));
        assert!(redacted.contains("[REDACTED PHONE]"));
        assert!(!redacted.contains("13812345678"));
    }

    #[test]
    fn detects_private_key() {
        let content = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAL...";
        let (_, categories) = scan_content(content);
        assert!(categories.contains(&"private_key"));
    }

    #[test]
    fn no_false_positives_on_normal_text() {
        let content = "今天天气很好，我们去公园散步吧。";
        let (_, categories) = scan_content(content);
        assert!(categories.is_empty(), "正常文本不应该触发敏感数据检测");
    }

    #[test]
    fn contains_sensitive_returns_bool() {
        assert!(contains_sensitive(
            "API_KEY=sk-abcdefghijklmnopqrstuvwxyz1234567890AB"
        ));
        assert!(!contains_sensitive("今天天气很好"));
    }
}
