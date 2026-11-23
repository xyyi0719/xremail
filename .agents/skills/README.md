# 已安装的 Agent Skills

这三个 skill 是从各自的公开仓库拉下来的标准 Claude Skill（`SKILL.md` 结构），
放在此目录供 agent 会话加载（`.agents/skills/<name>/SKILL.md`）。

| Skill | 来源 | 用途 |
|---|---|---|
| `ponytail` | github.com/DietrichGebert/ponytail | 极简开发纪律：最小完整改动，不为几行代码加依赖 |
| `taste-skill` | github.com/Leonxlnx/taste-skill (tasteskill.dev) | 反「AI 味」前端：设计规范 + 真实浏览器检查清单 |
| `gsap-*` | github.com/greensock/gsap-skills | GSAP 动画官方用法（core/react/scrolltrigger/timeline…） |

> 本环境的 agent 没有 `/plugin marketplace` 命令，所以以**仓库内 vendored** 方式安装。
> 这些是给 agent 读的规则与脚本，不参与应用构建（不打包进产物）。

## 本项目的实际取舍

- **taste-skill**：采纳了它的设计检查清单（equal edges / empty-area / one system /
  真实可访问性），用它审计并修了界面三处结构问题。
- **ponytail**：全程按它执行——例如**最终没有用 GSAP**（见下）。
- **gsap**：试用后发现为 3 个淡入效果要 +28KB gzip（112→140KB），
  而项目对首屏速度敏感。按 ponytail「不为几行代码加依赖」改用 **纯 CSS @keyframes**
  （0 字节，效果一致，含 `prefers-reduced-motion`）。GSAP 只保留 skill 文档供参考。

> **注意**：`taste-skill` 的 `assets/`（38MB 素材）与 `scripts/`（生成/检查脚本）
> **没有随附**——本仓库只采纳它的**设计规范与检查清单**（`SKILL.md` + `references/`），
> 不走它的「生成全新站点」流水线（那套是给空白项目用的，会重造整个工程）。
> 需要跑它的浏览器检查脚本时，从其上游仓库取。
