# Sunchain 文档索引

文档按业务域分子目录，避免平铺杂乱：

- **`Dex/`** — 去中心化交易所（订单簿 DEX、撮合、清算、喂价、索引、行情服务）。
  - `design/` 设计与分析；`implementation/` 落地记录与整改。
- **`Consensus/`** — 共识（SunLead 出块 / SFF 快速终局）。按阶段组织：
  - `Stage-1/` — **第一阶段（原型）**：在既有链模型下验证 SunLead 出块与 SFF 影子终局（F1–F3）。
    - `design/` 设计与规格：`SunLead-Deterministic-Leader-Development-Plan`、`SunLead-P2-8-Feasibility`、`Alpenglow-Inspired-Fast-Finality-Design`，以及形式化证明 `sff-f1-formal/`。
    - `implementation/` 落地记录：`SunLead-Remediation-Plan`、各阶段 Worklog（P0/P1/P2/P3）、`P3-TestPlan`、`SFF-Gossip-Delivery-Diagnosis`。
  - `Stage-2/` — **第二阶段（genesis 组合策略）**：在 genesis 时选定出块轴（BABE/SunLead）与终局轴（GRANDPA/SFF），四种组合各自组网。
    - `design/` 策略与上位文档：`Consensus-Genesis-Composition-Strategy`。
    - `implementation/` 暂空（Stage 0–3 推进记录待补）。
- **`General/`** — 通用：`rust-setup`、`Sunchain Admin CLI Design`。

## 阶段划分说明

- **第一阶段**：以"不替换原共识、按新链/影子模型验证"为前提的原型工作（SunLead 真替换 BABE 出块、SFF 为只读影子终局）。本阶段文档即 `Consensus/Stage-1/` 下全部内容。
- **第二阶段**：项目方 2026-11-05 决策——共识在 **genesis 时决策**、链生期不动态切换、无存量链需升级，最终形态为新旧两套共识在 genesis 任意组合均可组网。本阶段以 `Consensus/Stage-2/design/Consensus-Genesis-Composition-Strategy.md` 为总纲，内含对第一阶段文档的"取消 / 降级 / 保留"重定级（见该文档 §8）。

## 交叉引用约定

- 代码注释与各文档内部引用均已改为相对路径（如 `../Stage-1/design/...`）。
- 第一阶段文档统一以 `【2026-11-05 策略更新 / 状态回填】` 块指向上位策略文档；其中的行号是回填前快照，定位请按章节标题/关键字。
