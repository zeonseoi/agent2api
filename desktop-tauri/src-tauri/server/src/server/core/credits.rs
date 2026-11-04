//! 上游扣费 **Credits** 在 fork 侧的纯函数：把浮点余量夹成 0。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! Credits 的**解析**已由官方接管（`core::upstream::usage` 的 `first_amount`
//! 与别名表），本文件只剩报表侧那一个夹取函数 —— 官方文件里只留调用点，细则
//! 放这里，合并时那几处改动越少越好。

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
    use super::residue;

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
