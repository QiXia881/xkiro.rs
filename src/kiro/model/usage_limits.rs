//! 使用额度查询数据模型
//!
//! 包含 getUsageLimits API 的响应类型定义

use serde::Deserialize;

/// 使用额度查询响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsResponse {
    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 订阅信息
    #[serde(default)]
    pub subscription_info: Option<SubscriptionInfo>,

    /// 用户信息（含 email），用于回填凭据
    #[serde(default)]
    pub user_info: Option<UsageUserInfo>,

    /// 顶层 email（部分响应直接挂顶层）
    #[serde(default)]
    pub email: Option<String>,

    /// 超额配置 (overageConfiguration.overageStatus = ENABLED / DISABLED)
    #[serde(default)]
    pub overage_configuration: Option<OverageConfiguration>,

    /// 使用量明细列表
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,
}

/// 用户信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UsageUserInfo {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// 订阅信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    /// 订阅标题 (KIRO PRO+ / KIRO FREE 等)
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 订阅名称（部分响应不用 subscriptionTitle）
    #[serde(default)]
    pub subscription_name: Option<String>,

    /// 订阅类型（FREE / PRO / PRO_PLUS / POWER 等）
    #[serde(default)]
    pub subscription_type: Option<String>,

    /// 超额资格 (OVERAGE_CAPABLE / OVERAGE_INCAPABLE)
    #[serde(default)]
    pub overage_capability: Option<String>,
}

/// 超额配置
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverageConfiguration {
    /// 超额状态 (ENABLED / DISABLED)
    #[serde(default)]
    pub overage_status: Option<String>,
}

/// 使用量明细
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UsageBreakdown {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 奖励额度列表
    #[serde(default)]
    pub bonuses: Vec<Bonus>,

    /// 免费试用信息
    #[serde(default)]
    pub free_trial_info: Option<FreeTrialInfo>,

    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,

    /// 超额上限（KIRO Pro/Pro+ 才有，单位与 usage_limit 一致）
    #[serde(default)]
    pub overage_cap: f64,

    /// 超额上限（精确值）
    #[serde(default)]
    pub overage_cap_with_precision: f64,

    /// 超额调用单价
    #[serde(default)]
    pub overage_rate: f64,

    /// 当前超额消耗
    #[serde(default)]
    pub current_overages: f64,
}

/// 奖励额度
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bonus {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub status: Option<String>,
}

impl Bonus {
    /// 检查 bonus 是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

/// 免费试用信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FreeTrialInfo {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 免费试用过期时间 (Unix 时间戳)
    #[serde(default)]
    pub free_trial_expiry: Option<f64>,

    /// 免费试用状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub free_trial_status: Option<String>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

// ============ 便捷方法实现 ============

impl FreeTrialInfo {
    /// 检查免费试用是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

impl UsageLimitsResponse {
    /// 获取订阅标题
    pub fn subscription_title(&self) -> Option<&str> {
        let info = self.subscription_info.as_ref()?;
        info.subscription_title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                info.subscription_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
    }

    /// 获取规范化订阅类型（FREE / PRO / PRO_PLUS / POWER / ENTERPRISE / TEAMS）
    pub fn subscription_type(&self) -> Option<String> {
        let info = self.subscription_info.as_ref()?;
        let raw = info
            .subscription_title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                info.subscription_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
            .or_else(|| {
                info.subscription_type
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })?;
        Some(normalize_subscription_type(raw))
    }

    /// 获取超额资格 (OVERAGE_CAPABLE / OVERAGE_INCAPABLE)
    pub fn overage_capability(&self) -> Option<&str> {
        self.subscription_info
            .as_ref()
            .and_then(|info| info.overage_capability.as_deref())
    }

    /// 获取远端超额开关 (ENABLED / DISABLED)
    pub fn overage_status(&self) -> Option<&str> {
        self.overage_configuration
            .as_ref()
            .and_then(|cfg| cfg.overage_status.as_deref())
    }

    /// 获取超额上限（精确值优先，回退整数版本）
    ///
    /// 仅取主 breakdown 的 overageCap 字段，不含 trial/bonus。
    pub fn overage_cap(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };
        if breakdown.overage_cap_with_precision > 0.0 {
            breakdown.overage_cap_with_precision
        } else {
            breakdown.overage_cap
        }
    }

    pub fn overage_rate(&self) -> f64 {
        self.primary_breakdown()
            .map(|breakdown| breakdown.overage_rate)
            .unwrap_or_default()
    }

    pub fn current_overages(&self) -> f64 {
        self.primary_breakdown()
            .map(|breakdown| breakdown.current_overages)
            .unwrap_or_default()
    }

    /// 获取第一个使用量明细
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list.first()
    }

    /// 获取总使用限额（精确值）
    ///
    /// 累加基础额度、激活的免费试用额度和激活的奖励额度
    pub fn usage_limit(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total =
            numeric_with_precision(breakdown.usage_limit_with_precision, breakdown.usage_limit);

        // 累加激活的 free trial 额度
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total +=
                    numeric_with_precision(trial.usage_limit_with_precision, trial.usage_limit);
            }
        }

        // 累加激活的 bonus 额度
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.usage_limit;
            }
        }

        total
    }

    /// 获取总当前使用量（精确值）
    ///
    /// 累加基础使用量、激活的免费试用使用量和激活的奖励使用量
    pub fn current_usage(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = numeric_with_precision(
            breakdown.current_usage_with_precision,
            breakdown.current_usage,
        );

        // 累加激活的 free trial 使用量
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total +=
                    numeric_with_precision(trial.current_usage_with_precision, trial.current_usage);
            }
        }

        // 累加激活的 bonus 使用量
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.current_usage;
            }
        }

        total
    }

    pub fn primary_remaining(&self) -> f64 {
        (self.usage_limit() - self.current_usage()).max(0.0)
    }

    pub fn usage_ratio(&self) -> f64 {
        let usage_limit = self.usage_limit();
        if usage_limit > 0.0 {
            self.current_usage() / usage_limit
        } else {
            0.0
        }
    }

    pub fn primary_overage_used(&self) -> f64 {
        (self.current_usage() - self.usage_limit()).max(0.0)
    }

    pub fn current_overages_or_primary(&self) -> f64 {
        let current_overages = self.current_overages();
        if current_overages > 0.0 {
            current_overages
        } else {
            self.primary_overage_used()
        }
    }

    pub fn primary_overage_remaining(&self) -> f64 {
        if self.overage_status() == Some("ENABLED") {
            (self.overage_cap() - self.primary_overage_used()).max(0.0)
        } else {
            0.0
        }
    }

    pub fn trial_usage_current(&self) -> Option<f64> {
        let trial = self.primary_breakdown()?.free_trial_info.as_ref()?;
        Some(numeric_with_precision(
            trial.current_usage_with_precision,
            trial.current_usage,
        ))
    }

    pub fn trial_usage_limit(&self) -> Option<f64> {
        let trial = self.primary_breakdown()?.free_trial_info.as_ref()?;
        Some(numeric_with_precision(
            trial.usage_limit_with_precision,
            trial.usage_limit,
        ))
    }

    pub fn trial_status(&self) -> Option<&str> {
        self.primary_breakdown()?
            .free_trial_info
            .as_ref()?
            .free_trial_status
            .as_deref()
    }

    pub fn trial_expires_at(&self) -> Option<i64> {
        let expires_at = self
            .primary_breakdown()?
            .free_trial_info
            .as_ref()?
            .free_trial_expiry?;
        if expires_at > 0.0 {
            Some(expires_at.floor() as i64)
        } else {
            None
        }
    }
}

fn numeric_with_precision(precision: f64, base: f64) -> f64 {
    if precision > 0.0 { precision } else { base }
}

pub(crate) fn normalize_subscription_type(raw: &str) -> String {
    let upper = raw.to_ascii_uppercase();
    if upper.contains("PRO_PLUS") || upper.contains("PROPLUS") || upper.contains("PRO+") {
        return "PRO_PLUS".to_string();
    }
    if upper.contains("POWER") {
        return "POWER".to_string();
    }
    if upper.contains("ENTERPRISE") {
        return "ENTERPRISE".to_string();
    }
    if upper.contains("TEAMS") {
        return "TEAMS".to_string();
    }
    if upper.contains("PRO") {
        return "PRO".to_string();
    }
    "FREE".to_string()
}

#[cfg(test)]
mod tests {
    use super::UsageLimitsResponse;

    #[test]
    fn parses_overage_fields_from_usage_limits() {
        let usage: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "subscriptionInfo": {
                "subscriptionTitle": "KIRO PRO+",
                "overageCapability": "OVERAGE_CAPABLE"
            },
            "overageConfiguration": {
                "overageStatus": "ENABLED"
            },
            "usageBreakdownList": [{
                "currentUsageWithPrecision": 12.5,
                "usageLimitWithPrecision": 10.0,
                "overageCapWithPrecision": 50.0,
                "overageRate": 0.04,
                "currentOverages": 2.5
            }]
        }))
        .expect("usage limits should parse");

        assert_eq!(usage.overage_status(), Some("ENABLED"));
        assert_eq!(usage.overage_capability(), Some("OVERAGE_CAPABLE"));
        assert_eq!(usage.subscription_type().as_deref(), Some("PRO_PLUS"));
        assert_eq!(usage.overage_cap(), 50.0);
        assert_eq!(usage.overage_rate(), 0.04);
        assert_eq!(usage.current_overages(), 2.5);
        assert_eq!(usage.primary_remaining(), 0.0);
        assert_eq!(usage.usage_ratio(), 1.25);
        assert_eq!(usage.primary_overage_used(), 2.5);
        assert_eq!(usage.current_overages_or_primary(), 2.5);
        assert_eq!(usage.primary_overage_remaining(), 47.5);
    }

    #[test]
    fn usage_limits_plain_fields_and_subscription_fallback_are_normalized() {
        let usage: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "subscriptionInfo": {
                "subscriptionName": "KIRO POWER"
            },
            "usageBreakdownList": [{
                "currentUsage": 7.5,
                "usageLimit": 20.0,
                "freeTrialInfo": {
                    "currentUsage": 1.5,
                    "usageLimit": 5.0,
                    "freeTrialStatus": "ACTIVE",
                    "freeTrialExpiry": 1893555000.9
                }
            }]
        }))
        .expect("usage limits should parse plain numeric fields");

        assert_eq!(usage.subscription_title(), Some("KIRO POWER"));
        assert_eq!(usage.subscription_type().as_deref(), Some("POWER"));
        assert_eq!(usage.current_usage(), 9.0);
        assert_eq!(usage.usage_limit(), 25.0);
        assert_eq!(usage.primary_remaining(), 16.0);
        assert_eq!(usage.usage_ratio(), 9.0 / 25.0);
        assert_eq!(usage.primary_overage_used(), 0.0);
        assert_eq!(usage.current_overages_or_primary(), 0.0);
        assert_eq!(usage.trial_usage_current(), Some(1.5));
        assert_eq!(usage.trial_usage_limit(), Some(5.0));
        assert_eq!(usage.trial_status(), Some("ACTIVE"));
        assert_eq!(usage.trial_expires_at(), Some(1_893_555_000));
    }
}
