//! 上游扣费 **Credits** 在 fork 侧的纯函数：从 `usage` 里解析、把浮点余量夹成 0。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! Credits 是本 fork 加的旁路读数，官方文件（`core::upstream::usage`、
//! `request_stats::{report,record,...}`）里只留字段与调用点，细则放这里 ——
//! 合并时那几处改动越少越好。

use serde_json::Value;

/// 从 `usage` 对象里取本次扣费；缺失或非数值按 0。
///
/// ── 三种别名 ────────────────────────────────────────────────
/// 不同家 / 不同版本对「本次扣费」的命名不统一，对照 buddy-proxy 取
/// `credit` / `credits` / `cost`，取到哪个算哪个。都没有 = 这次没上报，
/// 记 0 —— 不给 0 编一个「免费」的语义。
pub fn credit_of(object: &serde_json::Map<String, Value>) -> f64 {
    float_field(object.get("credit"))
        .or_else(|| float_field(object.get("credits")))
        .or_else(|| float_field(object.get("cost")))
        .unwrap_or(0.0)
}

/// 取浮点数值字段：整数 / 浮点 / 数字字符串都认（Credits 是小数，不能用
/// 取整的 `number_field`），非数值与非有限值返回 None。
fn float_field(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|number| number.is_finite())
}

/// 未被任何组覆盖的 Credits 余量（`总量 − 各组之和`），并把浮点舍入残值夹成 0。
///
/// ── 为什么必须夹 ────────────────────────────────────────────
/// 各组的 credits 是逐条累加的浮点数，其和与总量的差在数学上恒为 0，浮点下
/// 却是 1e-14 级的噪声。这个噪声值本身无害，但它是**真值 > 0**：报表的
/// Credits 环形图按 `credits > 0` 决定画不画一段，于是噪声会凭空多出一段
/// 「未知 0 0.0%」—— 读数被 toFixed(2) 舍成 0，图例却留着那一行。
/// 真正的余量（旧聚合行没有 providerStats / accountStats 键时，那段请求的
/// credits 会整个落进来）是 0.01 以上的量级，与噪声隔着十几个数量级。
/// 取 1e-6 作阈值：远高于任何规模的累加噪声（哪怕是百万级的 credits），
/// 又远低于一分 credit —— 两者之间没有任何真实取值。
pub fn residue(total: f64, covered: f64) -> f64 {
    let residue = total - covered;
    if residue.abs() < 1e-6 {
        0.0
    } else {
        residue
    }
}

#[cfg(test)]
mod tests {
    use super::{credit_of, residue};
    use serde_json::{json, Value};

    fn object(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    /// 三种别名都认，数字字符串也认。
    #[test]
    fn credit_aliases_are_recognized() {
        assert_eq!(credit_of(&object(json!({ "credit": 0.07 }))), 0.07);
        assert_eq!(credit_of(&object(json!({ "credits": 5.3 }))), 5.3);
        assert_eq!(credit_of(&object(json!({ "cost": 12 }))), 12.0);
        assert_eq!(credit_of(&object(json!({ "credit": "0.25" }))), 0.25);
    }

    /// 缺失 / 非数值 / 非有限值都按 0（不给 0 编「免费」的语义）。
    #[test]
    fn missing_or_invalid_credit_is_zero() {
        assert_eq!(credit_of(&object(json!({}))), 0.0);
        assert_eq!(credit_of(&object(json!({ "credit": null }))), 0.0);
        assert_eq!(credit_of(&object(json!({ "credit": "abc" }))), 0.0);
        assert_eq!(credit_of(&object(json!({ "credit": true }))), 0.0);
    }

    /// 纯浮点噪声必须夹成 0：报表的 Credits 环形图按 `credits > 0` 取段，
    /// 2.8e-14 会凭空多出一行「未知 0 0.0%」（实测复现过，见函数注释）。
    #[test]
    fn residue_clamps_floating_noise() {
        // 库里实测的那一组：各组之和与总量的差是 1e-14 级
        assert_eq!(residue(120.2500000000001, 120.25000000000007), 0.0);
        // 正负两侧的噪声都夹
        assert_eq!(residue(100.0, 100.0 + 1e-9), 0.0);
        assert_eq!(residue(100.0, 100.0 - 1e-9), 0.0);
    }

    /// 真正的余量（旧聚合行没有 providerStats / accountStats 键时整段落进来）
    /// 是 0.01 以上的量级，**不能**被夹掉 —— 夹了就丢数据了。
    #[test]
    fn residue_keeps_real_remainder() {
        assert_eq!(residue(120.25, 100.0), 20.25);
        // 一分 credit 是真实可读的最小量级，阈值必须远低于它。
        // 余量本身带浮点尾数（`1.0 - 0.99` → `0.010000000000000009`），
        // 所以这里比量级而不是比字面相等 —— 关键是被**保留**（> 0，没夹成 0）
        let tiny = residue(1.0, 0.99);
        assert!(tiny > 0.0, "真实余量不该被夹掉，得到 {tiny}");
        assert!((tiny - 0.01).abs() < 1e-9);
    }
}
