#!/usr/bin/env bash
# 验证 deepin 25.3 上「运行中的 X 会话里热插拔一块 evdi 虚拟显卡」会不会让桌面卡死，
# 以及卡死后让 kwin_x11 重新接管能不能把桌面救回来（用来决定安装包能否免重启）。
#
# 用法：
#   tools/evdi-hotplug-test.sh              完整测试：热插拔 → 探测 → 尝试恢复
#   tools/evdi-hotplug-test.sh --snapshot   只采集当前状态，不动显卡
#   tools/evdi-hotplug-test.sh --recover    桌面已经卡死时从 tty 运行：采集现场 → 尝试恢复
#   tools/evdi-hotplug-test.sh --no-kwin    不自动重启 kwin（只采集）
#
# 结果写入：$HOME/androscreen-hotplug-test-<时间>.txt（同目录还有 *.xorg.log 拷贝）
#
# 说明：
#   * 脚本不卸载现有的 evdi 卡（它被 Xorg 占着），而是写 /sys/devices/evdi/add
#     新增一块虚拟卡；Xorg 走的是同一条「运行中热插拔 DRM 显卡」的路径。
#   * 测试会让桌面可能立刻冻结，请先保存工作。
#   * 测试后重启一次系统，把多出来的那块虚拟卡清掉。

set -u

MODE=test
DO_KWIN=1

usage() {
    sed -n '2,17p' "$0"
}

for arg in "$@"; do
    case $arg in
        --snapshot) MODE=snapshot ;;
        --recover)  MODE=recover ;;
        --no-kwin)  DO_KWIN=0 ;;
        -h|--help)  usage; exit 0 ;;
        *) echo "未知参数: $arg（--help 查看用法）" >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -eq 0 ]; then
    echo "请用普通用户运行；需要 root 的步骤脚本会自己调 sudo。" >&2
    exit 1
fi

OUT="$HOME/androscreen-hotplug-test-$(date +%Y%m%d-%H%M%S).txt"
XORG_LOG_COPY="$OUT.xorg.log"
export DISPLAY="${DISPLAY:-:0}"
export XAUTHORITY="${XAUTHORITY:-$HOME/.Xauthority}"

XORG_PID=$(pgrep -x Xorg | head -n1)
KWIN_PID=$(pgrep -x kwin_x11 | head -n1)
SHELL_PIDS=$(pgrep -x dde-shell | tr '\n' ' ')

: >"$OUT"

# ---------- 输出辅助 ----------

both() {
    # 同时写屏幕和报告
    printf '%s\n' "$*" | tee -a "$OUT"
}

log() {
    printf '%s\n' "$*" >>"$OUT"
}

section() {
    printf '\n\n========== %s ==========\n' "$*" >>"$OUT"
    printf '== %s\n' "$*"
}

block() {
    # block <标签> <函数或命令...>
    local label=$1
    shift
    {
        printf '\n--- %s ---\n' "$label"
        if declare -F "$1" >/dev/null 2>&1; then
            # shell 函数没法交给 timeout，直接调用
            "$@"
        else
            timeout 20 "$@"
        fi
        printf '[exit=%s]\n' "$?"
    } >>"$OUT"
}

warm_sudo() {
    sudo -n true 2>/dev/null && return 0
    printf '采集线程栈需要 root，请输入 sudo 密码：' >&2
    sudo -v
}

x_alive() {
    timeout 3 xrandr --query >/dev/null 2>&1
}

# ---------- 状态采集 ----------

show_evdi() {
    lsmod | grep -E '^(evdi|uinput)' || echo '(evdi / uinput 未加载)'
    echo "evdi count = $(cat /sys/devices/evdi/count 2>/dev/null || echo '不可读')"
    echo '--- /sys/class/drm ---'
    ls -l /sys/class/drm
    echo '--- /dev/dri ---'
    ls -l /dev/dri 2>&1
}

show_procs() {
    local pids="$XORG_PID $KWIN_PID $SHELL_PIDS"
    # 去掉空值
    pids=$(echo "$pids" | tr ' ' '\n' | grep -E '^[0-9]+$' | tr '\n' ' ')
    echo "Xorg=$XORG_PID kwin_x11=$KWIN_PID dde-shell=$SHELL_PIDS"
    ps -o pid,ppid,stat,wchan:28,etime,time,comm -p $pids 2>&1
    for p in $pids; do
        printf 'PID %s (%s) threads=%s\n' "$p" "$(cat /proc/"$p"/comm 2>/dev/null)" \
            "$(awk '/^Threads:/{print $2}' /proc/"$p"/status 2>/dev/null)"
    done
}

show_xorg_fds() {
    sudo -n ls -l /proc/"$XORG_PID"/fd 2>&1 | grep -E 'dri|card' || echo '(看不到，或没有 drm fd)'
}

show_kernel() {
    sudo -n dmesg --ctime 2>/dev/null | tail -n 30 || journalctl -k -n 30 --no-pager 2>&1
}

show_journal() {
    journalctl -b -n 40 --no-pager 2>&1 || sudo -n journalctl -b -n 40 --no-pager 2>&1
}

snapshot_state() {
    local tag=$1
    section "状态快照 · $tag · $(date '+%F %T')"
    block 'uname / uptime' bash -c 'uname -r; uptime'
    block '会话' bash -c 'loginctl show-session "${XDG_SESSION_ID:-1}" -p Type -p Active -p State 2>&1; echo; who'
    block 'evdi / drm' show_evdi
    block '关键进程' show_procs
    block 'xrandr --query' timeout 5 xrandr --query
    block 'xrandr --listproviders' timeout 5 xrandr --listproviders
    block 'Xorg 打开的 drm 设备' show_xorg_fds
    block '最近内核日志' show_kernel
}

capture_stacks() {
    section '线程现场'
    warm_sudo
    local pids="$XORG_PID $KWIN_PID $SHELL_PIDS"
    pids=$(echo "$pids" | tr ' ' '\n' | grep -E '^[0-9]+$' | tr '\n' ' ')
    for p in $pids; do
        if [ ! -d "/proc/$p" ]; then
            log "PID $p 已不存在"
            continue
        fi
        log ""
        log "######## PID $p ($(cat /proc/"$p"/comm 2>/dev/null)) ########"
        block "PID $p status" bash -c "grep -E '^(Name|State|Threads)' /proc/$p/status"
        local t tid wchan state
        for t in /proc/"$p"/task/*; do
            tid=${t##*/}
            wchan=$(cat "$t/wchan" 2>/dev/null)
            state=$(awk '/^State:/{print $2, $3}' "$t/status" 2>/dev/null)
            {
                printf '\n-- TID %s comm=%s state=%s wchan=%s\n' \
                    "$tid" "$(cat "$t/comm" 2>/dev/null)" "$state" "$wchan"
                printf 'syscall: '
                sudo -n cat "$t/syscall" 2>&1 | head -1
                printf 'stack:\n'
                sudo -n cat "$t/stack" 2>&1 | head -n 12
            } >>"$OUT"
        done
    done
}

collect_xorg_log() {
    section 'Xorg 日志'
    if sudo -n cp /var/log/Xorg.0.log "$XORG_LOG_COPY" 2>/dev/null; then
        both "已保存: $XORG_LOG_COPY"
        sudo -n tail -n 40 /var/log/Xorg.0.log >>"$OUT" 2>&1
    else
        log '(无法读取 /var/log/Xorg.0.log)'
    fi
}

# ---------- 测试动作 ----------

trigger_hotplug() {
    section '触发：新增一块 evdi 虚拟显卡'
    block '触发前 count' cat /sys/devices/evdi/count
    if ! timeout 20 sudo -n bash -c 'echo 1 > /sys/devices/evdi/add'; then
        both '!! 写入 /sys/devices/evdi/add 失败（evdi 未加载或没有 root）'
        return 1
    fi
    sleep 1
    block '触发后 count / 新设备' show_evdi
    return 0
}

probe_x() {
    section '热插拔后 12 秒内 X server 是否还响应'
    local i ok=0 fail=0
    for i in $(seq 1 12); do
        if x_alive; then
            ok=$((ok + 1))
            printf 't+%02ds  X=响应    kwin进程=%s\n' "$i" "$(pgrep -c kwin_x11)" >>"$OUT"
        else
            fail=$((fail + 1))
            printf 't+%02ds  X=无响应  kwin进程=%s\n' "$i" "$(pgrep -c kwin_x11)" >>"$OUT"
        fi
        sleep 1
    done
    both "12 次探测：X 响应 $ok 次，无响应 $fail 次"
    if [ "$fail" -ge 3 ]; then
        both '判断：X server 本身卡住了 —— 重启 kwin 也救不回来。'
    else
        both '判断：X server 仍在响应 —— 若屏幕冻结，则是合成器层。'
    fi
}

try_kwin_restart() {
    section '尝试恢复：重启 kwin_x11（画面会闪一下）'
    block '重启前 kwin' pgrep -a kwin_x11
    ( setsid env DISPLAY="$DISPLAY" XAUTHORITY="$XAUTHORITY" \
        kwin_x11 --replace >/dev/null 2>&1 & )
    sleep 6
    block '重启后 kwin' pgrep -a kwin_x11
    block '窗口管理器探针' timeout 5 xprop -root _NET_SUPPORTING_WM_CHECK
    if x_alive; then both 'X 仍在响应。'; else both 'X 仍然无响应。'; fi
}

ask_observation() {
    section '人工确认'
    both '请看屏幕，确认这两件事：'
    both '  1) 现在桌面恢复了吗（鼠标能动、窗口能响应）？'
    both '  2) 热插拔之后，屏幕是不是冻结在最后一帧？'
    if x_alive; then
        printf '桌面现在恢复了吗？[y=恢复 / n=还是卡死 / 回车=不确定] ' >&2
        if read -r -t 60 answer; then
            log "用户观察: ${answer:-未回答}"
        else
            log '用户观察: 超时未回答'
        fi
    else
        log '用户观察: X 无响应，无法输入'
    fi
}

# ---------- 主流程 ----------

both "报告文件: $OUT"
both "模式: $MODE   kwin: $([ "$DO_KWIN" -eq 1 ] && echo 会尝试重启 || echo 不重启)"

case $MODE in
    snapshot)
        section '采集当前状态（不触发热插拔）'
        snapshot_state '当前'
        capture_stacks
        collect_xorg_log
        block 'journal 最近 40 行' show_journal
        ;;
    recover)
        section '采集卡死现场'
        snapshot_state '卡死现场'
        capture_stacks
        collect_xorg_log
        block 'journal 最近 40 行' show_journal
        [ "$DO_KWIN" -eq 1 ] && try_kwin_restart
        snapshot_state '恢复尝试后'
        ask_observation
        ;;
    test)
        both ''
        both '这个测试会往运行中的 X 会话里热插拔一块 evdi 虚拟显卡。'
        both '在 deepin 25.3 上，桌面可能立刻冻结（这正是要验证的现象）。'
        both '请先保存所有工作。'
        both ''
        printf '继续？[y/N] ' >&2
        read -r ans
        case ${ans:-n} in
            y|Y) ;;
            *) echo '已取消'; exit 0 ;;
        esac
        echo '需要 root 写 sysfs，请输入 sudo 密码：' >&2
        if ! sudo -v; then
            echo 'sudo 校验失败，退出。' >&2
            exit 1
        fi
        snapshot_state '测试前'
        collect_xorg_log
        if trigger_hotplug; then
            probe_x
            snapshot_state '热插拔后'
            capture_stacks
            block '热插拔后的内核日志' show_kernel
            [ "$DO_KWIN" -eq 1 ] && try_kwin_restart
            snapshot_state '恢复尝试后'
            collect_xorg_log
            block 'journal 最近 40 行' show_journal
            ask_observation
        fi
        both ''
        both '测试结束。'
        both '如果多出了一块虚拟显卡（/sys/class/drm 里多了 card2），重启一次即可清掉：sudo reboot'
        both '若桌面仍然卡死：Ctrl+Alt+F3 切到 tty 登录，执行 sudo systemctl restart lightdm（会关掉所有窗口）。'
        ;;
esac

both ''
both "报告已写入: $OUT"
