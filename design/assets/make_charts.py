# Charts for the checkpoint compute-gas accounting design doc.
# Single-hue emphasis scheme: neutral gray for context bars, blue (#2a78d6) for
# the highlighted entity, hatch texture for the pathological case. Recessive
# grid, no top/right spines, direct value labels, text in near-black ink.
#
# Emits every figure in two languages: `checkpoint-figN-*.png` (Chinese, used by
# the design doc) and `checkpoint-figN-*-en.png` (English).
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager

# CJK font setup (macOS)
for name in ["PingFang SC", "Hiragino Sans GB", "Arial Unicode MS"]:
    if any(f.name == name for f in font_manager.fontManager.ttflist):
        plt.rcParams["font.sans-serif"] = [name, "DejaVu Sans"]
        break
plt.rcParams["axes.unicode_minus"] = False

BLUE = "#2a78d6"
BLUE_DARK = "#104281"
GRAY = "#b3b1a7"
GRAY_DARK = "#6f6d66"
INK = "#1a1a19"
INK_2 = "#5c5a52"
GRID = "#e6e4dc"

import os

OUT = os.path.dirname(os.path.abspath(__file__))

TEXT = {
    "zh": {
        "fig1_ylabel_a": "占全程序 cycles（%）",
        "fig1_title_a": "最热的廉价操作码吃掉的周期",
        "fig1_parts": ["包装结构\n+ 记账", "单条限额\n检查跳转 jb", "操作码本体\n（其余）"],
        "fig1_ylabel_b": "push1 内部占比（Ir / 采样归因）",
        "fig1_title_b": "push1 内部：一半以上是计量税",
        "fig1_suptitle": "逐操作码计量税解剖 —— 包装成本归零的全程序天花板：5–9% cycles",
        "fig2_ylabel_a": "残余包装税（占执行操作码 %）",
        "fig2_title_a": "税：粒度越细，重新上税越多",
        "fig2_ylabel_b": "最大段长（k gas，主网观测）",
        "fig2_title_b": "界：V1 观测值是假象 —— 结构上无界",
        "fig2_annot": "对抗形状（纯算术循环）下\nV1 段长 = 剩余 EVM gas，无结构界",
        "fig2_annot_xytext": (0.62, 30.5),
        "fig2_annot_va": "baseline",
        "fig2_suptitle": "检查点粒度两难（主网 1,000 笔轨迹重切）：残余税与段长上界不可兼得 —— 除非换执法机制",
        "fig3_schemes": [
            "Rex5\n逐操作码（现行）",
            "V1 检查点\n（帧末结算）",
            "V1.5\n（回跳结算）",
            "V0 gas 钳制\n（最终方案）",
        ],
        "fig3_ylabel": "halt 时已记账 compute（k gas，log）",
        "fig3_title": "执法精确性：detention cap 下 26 万 gas 纯算术循环的 halt 落点\n"
        "（V1 overshoot 到帧末；V0 停在越限操作码执行前，零 overshoot）",
        "fig4_schemes": ["逐操作码\n（改造前）", "V2", "V1.5", "V1", "V0\n（最终）", "原装 revm\n（下界）"],
        "fig4_ylabel_a": "热循环用时（ms）",
        "fig4_title_a": "解释器热循环（70 万廉价操作码）：-50%，落在地板上",
        "fig4_labels_b": ["逐操作码\n（rex5）", "V0\n（最终）", "原装 revm\n（下界）"],
        "fig4_ylabel_b": "weth9 transfer 用时（μs）",
        "fig4_title_b": "真实 ERC20 转账（同 run 对照）",
        "fig4_suptitle": "最终效果（本地 wall-clock，同 run 内多方案对照；V0 = 检查点结算 + gas 钳制执法）",
    },
    "en": {
        "fig1_ylabel_a": "Share of whole-program cycles (%)",
        "fig1_title_a": "Cycles consumed by the hottest cheap opcodes",
        "fig1_parts": ["Wrapper\n+ accounting", "Per-op limit-check\nbranch (jb)", "Opcode body\n(rest)"],
        "fig1_ylabel_b": "Breakdown inside push1 (Ir / sampled attribution)",
        "fig1_title_b": "Inside push1: more than half is metering tax",
        "fig1_suptitle": "Anatomy of the per-opcode metering tax — whole-program ceiling with wrapper cost at zero: 5–9% of cycles",
        "fig2_ylabel_a": "Residual wrapper tax (% of executed opcodes)",
        "fig2_title_a": "Tax: finer granularity re-taxes more",
        "fig2_ylabel_b": "Max segment length (k gas, mainnet observed)",
        "fig2_title_b": "Bound: V1's observed value is an illusion",
        "fig2_annot": "Adversarial shape (pure arithmetic loop):\nV1 segment = all remaining EVM gas,\nno structural bound",
        "fig2_annot_xytext": (0.55, 33.3),
        "fig2_annot_va": "top",
        "fig2_suptitle": "Checkpoint granularity dilemma (1,000 mainnet traces): residual tax vs segment bound — unless enforcement changes",
        "fig3_schemes": [
            "Rex5\nper-opcode (current)",
            "V1 checkpoints\n(frame-end settle)",
            "V1.5\n(backward-jump settle)",
            "V0 gas clamp\n(final)",
        ],
        "fig3_ylabel": "Compute recorded at halt (k gas, log)",
        "fig3_title": "Enforcement exactness: halt point of a 260k-gas pure arithmetic loop under a detention cap\n"
        "(V1 overshoots to frame end; V0 stops before the crossing opcode executes — zero overshoot)",
        "fig4_schemes": ["Per-opcode\n(before)", "V2", "V1.5", "V1", "V0\n(final)", "Vanilla revm\n(floor)"],
        "fig4_ylabel_a": "Hot-loop time (ms)",
        "fig4_title_a": "Interpreter hot loop (700k cheap opcodes): -50%, landing on the floor",
        "fig4_labels_b": ["Per-opcode\n(rex5)", "V0\n(final)", "Vanilla revm\n(floor)"],
        "fig4_ylabel_b": "weth9 transfer time (μs)",
        "fig4_title_b": "Real ERC20 transfer (same-run comparison)",
        "fig4_suptitle": "Final effect (local wall-clock, schemes compared within one run; V0 = checkpoint settlement + gas-clamp enforcement)",
    },
}


def style_ax(ax, ymax=None):
    ax.spines[["top", "right"]].set_visible(False)
    ax.spines[["left", "bottom"]].set_color(GRAY)
    ax.tick_params(colors=INK_2, labelsize=9)
    ax.yaxis.grid(True, color=GRID, linewidth=0.8, zorder=0)
    ax.set_axisbelow(True)
    if ymax:
        ax.set_ylim(0, ymax)


def bar_labels(ax, bars, fmt, dy=0.02, fontsize=9):
    top = ax.get_ylim()[1]
    for b in bars:
        ax.text(
            b.get_x() + b.get_width() / 2,
            b.get_height() + top * dy,
            fmt(b.get_height()),
            ha="center",
            va="bottom",
            fontsize=fontsize,
            color=INK,
        )


def fig1(t, suffix):
    # The per-opcode metering tax: where the cycles go.
    fig, (a, b) = plt.subplots(1, 2, figsize=(9.2, 3.4), dpi=160)

    ops = ["push1", "add", "pop"]
    share = [18.27, 4.79, 4.09]
    bars = a.bar(ops, share, width=0.52, color=[BLUE, GRAY, GRAY], zorder=3)
    style_ax(a, ymax=22)
    bar_labels(a, bars, lambda v: f"{v:.2f}%")
    a.set_ylabel(t["fig1_ylabel_a"], fontsize=9, color=INK_2)
    a.set_title(t["fig1_title_a"], fontsize=10.5, color=INK, pad=10)

    vals = [27, 25, 48]
    colors = [BLUE, BLUE_DARK, GRAY]
    bars = b.bar(t["fig1_parts"], vals, width=0.52, color=colors, zorder=3)
    style_ax(b, ymax=58)
    bar_labels(b, bars, lambda v: f"≈{v:.0f}%")
    b.set_ylabel(t["fig1_ylabel_b"], fontsize=9, color=INK_2)
    b.set_title(t["fig1_title_b"], fontsize=10.5, color=INK, pad=10)

    fig.suptitle(t["fig1_suptitle"], fontsize=11.5, color=INK, y=1.04)
    fig.tight_layout()
    fig.savefig(f"{OUT}/checkpoint-fig1-per-opcode-tax{suffix}.png", bbox_inches="tight")
    plt.close(fig)


def fig2(t, suffix):
    # Granularity dilemma: residual tax vs segment bound (mainnet n=1000).
    fig, (a, b) = plt.subplots(1, 2, figsize=(9.2, 3.6), dpi=160)

    variants = ["V1", "V1.5", "V2", "V3"]
    tax = [0.71, 3.87, 7.80, 14.39]
    colors = [BLUE, BLUE, GRAY, GRAY]
    bars = a.bar(variants, tax, width=0.5, color=colors, zorder=3)
    style_ax(a, ymax=17)
    bar_labels(a, bars, lambda v: f"{v:.2f}%")
    a.set_ylabel(t["fig2_ylabel_a"], fontsize=9, color=INK_2)
    a.set_title(t["fig2_title_a"], fontsize=10.5, color=INK, pad=10)

    seg_max = [27.151, 6.861, 6.772, 6.771]
    bars = b.bar(variants, seg_max, width=0.5, color=[GRAY, BLUE, GRAY, GRAY], zorder=3)
    bars[0].set_hatch("///")
    bars[0].set_edgecolor(GRAY_DARK)
    style_ax(b, ymax=34)
    bar_labels(b, bars, lambda v: f"{v:,.1f}k")
    b.set_ylabel(t["fig2_ylabel_b"], fontsize=9, color=INK_2)
    b.set_title(t["fig2_title_b"], fontsize=10.5, color=INK, pad=10)
    b.annotate(
        t["fig2_annot"],
        xy=(0, 27.8),
        xytext=t["fig2_annot_xytext"],
        va=t["fig2_annot_va"],
        fontsize=8.5,
        color=INK,
        arrowprops=dict(arrowstyle="->", color=GRAY_DARK, lw=0.9),
    )

    fig.suptitle(t["fig2_suptitle"], fontsize=11.5, color=INK, y=1.04)
    fig.tight_layout()
    fig.savefig(f"{OUT}/checkpoint-fig2-granularity{suffix}.png", bbox_inches="tight")
    plt.close(fig)


def fig3(t, suffix):
    # Enforcement exactness: recorded compute at halt, detention cap scenario.
    fig, ax = plt.subplots(figsize=(7.2, 3.6), dpi=160)

    halted_at = [22.0, 281.0, 22.0, 22.0]
    colors = [GRAY, GRAY, GRAY, BLUE]
    bars = ax.bar(t["fig3_schemes"], halted_at, width=0.5, color=colors, zorder=3)
    bars[1].set_hatch("///")
    bars[1].set_edgecolor(GRAY_DARK)
    ax.set_yscale("log")
    ax.set_ylim(10, 700)
    ax.spines[["top", "right"]].set_visible(False)
    ax.spines[["left", "bottom"]].set_color(GRAY)
    ax.tick_params(colors=INK_2, labelsize=9)
    ax.yaxis.grid(True, color=GRID, linewidth=0.8, zorder=0)
    ax.set_axisbelow(True)
    for b_, v in zip(bars, halted_at):
        ax.text(
            b_.get_x() + b_.get_width() / 2,
            v * 1.12,
            f"{v:.0f}k",
            ha="center",
            va="bottom",
            fontsize=9.5,
            color=INK,
        )
    ax.axhline(22.0, color=BLUE_DARK, linestyle="--", linewidth=1.2, zorder=2)
    ax.text(3.42, 19.0, "detention cap", fontsize=8.5, color=BLUE_DARK, ha="right")
    ax.set_ylabel(t["fig3_ylabel"], fontsize=9, color=INK_2)
    ax.set_title(t["fig3_title"], fontsize=10.5, color=INK, pad=10)
    fig.tight_layout()
    fig.savefig(f"{OUT}/checkpoint-fig3-enforcement{suffix}.png", bbox_inches="tight")
    plt.close(fig)


def fig4(t, suffix):
    # Final effect: wall-clock per scheme.
    fig, (a, b) = plt.subplots(
        1, 2, figsize=(9.6, 3.7), dpi=160, gridspec_kw={"width_ratios": [1.35, 1]}
    )

    times = [1.87, 1.09, 1.11, 0.93, 0.93, 0.94]
    colors = [GRAY, GRAY, GRAY, GRAY, BLUE, GRAY]
    bars = a.bar(t["fig4_schemes"], times, width=0.55, color=colors, zorder=3)
    bars[-1].set_alpha(0.45)
    style_ax(a, ymax=2.2)
    bar_labels(a, bars, lambda v: f"{v:.2f}")
    a.set_ylabel(t["fig4_ylabel_a"], fontsize=9, color=INK_2)
    a.set_title(t["fig4_title_a"], fontsize=10.5, color=INK, pad=10)
    a.annotate(
        "-50.3%",
        xy=(4, 0.93),
        xytext=(2.35, 1.62),
        fontsize=11,
        color=BLUE_DARK,
        fontweight="bold",
        arrowprops=dict(arrowstyle="->", color=BLUE_DARK, lw=1.1),
    )

    times_w = [9.40, 9.26, 8.86]
    bars = b.bar(t["fig4_labels_b"], times_w, width=0.5, color=[GRAY, BLUE, GRAY], zorder=3)
    bars[-1].set_alpha(0.45)
    style_ax(b, ymax=11)
    bar_labels(b, bars, lambda v: f"{v:.2f}")
    b.set_ylabel(t["fig4_ylabel_b"], fontsize=9, color=INK_2)
    b.set_title(t["fig4_title_b"], fontsize=10.5, color=INK, pad=10)

    fig.suptitle(t["fig4_suptitle"], fontsize=11.5, color=INK, y=1.04)
    fig.tight_layout()
    fig.savefig(f"{OUT}/checkpoint-fig4-final-effect{suffix}.png", bbox_inches="tight")
    plt.close(fig)


for lang, suffix in [("zh", ""), ("en", "-en")]:
    t = TEXT[lang]
    fig1(t, suffix)
    fig2(t, suffix)
    fig3(t, suffix)
    fig4(t, suffix)

print("charts written")
