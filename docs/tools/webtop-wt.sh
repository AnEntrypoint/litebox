#! /bin/bash

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# wt.sh <rung 1-4> <hold secs> [extra chromium flags]
# Boots the webtop stack rung by rung inside litebox.  Captures go to stdout as "XWD<n>" / "CXWD<n>"
# blocks (raw xwd); the host decodes them.  Every X client runs under a 25 s watchdog, so a wedged
# X server costs one capture, never the whole run.
R=$1; HOLD=$2; EXTRA="$3"
export HOME=/tmp/home XDG_RUNTIME_DIR=/tmp/xdg XDG_CONFIG_HOME=/tmp/home/.config XDG_CACHE_HOME=/tmp/home/.cache XDG_DATA_HOME=/tmp/home/.local/share DISPLAY=:0 LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu:/lib64:/lib:/opt/lib
mkdir -p $HOME/.config $HOME/.cache $HOME/.local/share $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
guard() {
  "$@" &
  p=$!; i=0
  while kill -0 $p 2>/dev/null; do
    if [ $i -ge 25 ]; then kill -9 $p 2>/dev/null; echo "[wt] guard killed after 25s: $1 $2"; break; fi
    sleep 1; i=$((i+1))
  done
  wait $p 2>/dev/null
  return 0
}
echo "[wt] rung=$R hold=$HOLD extra=[$EXTRA]"
echo "[hb] boot" >&2
/usr/bin/Xvfb :0 -screen 0 1024x768x24 -nolisten tcp -ac -retro -noreset -extension MIT-SHM >/tmp/home/xvfb.log 2>&1 &
sleep 60
echo "[wt] X0 assumed up after 60s"
echo "[wt] X0 assumed up after 60s"
if [ $R -ge 2 ] && [ $R -lt 3 ]; then /usr/bin/xfwm4 --display :0 --replace --compositor=off >/tmp/home/xfwm4.log 2>&1 & echo "[wt] xfwm4 spawned"; sleep 5; fi
if [ $R -ge 3 ]; then /usr/bin/xfce4-session >/tmp/home/session.log 2>&1 & echo "[wt] xfce4-session spawned"; fi
if [ $R -ge 4 ]; then sleep 20; /usr/lib/chromium/chromium $EXTRA --ozone-platform=x11 --no-first-run --disable-breakpad --disable-crash-reporter --no-default-browser-check --user-data-dir=/tmp/home/udd --window-size=1024,768 --enable-logging=stderr file:///page.html >/tmp/home/chrome.out 2>/tmp/home/chrome.err & echo "[wt] chromium spawned"; fi
t=0
while [ $t -lt $HOLD ]; do
  echo "[hb] loop t=$t" >&2
  sleep 30; t=$((t+30))
  guard xwd -root -silent -out /tmp/home/root.xwd
  n=$(wc -c < /tmp/home/root.xwd 2>/dev/null || echo 0)
  echo "[wt] t=$t root_xwd_bytes=$n"
  guard xwininfo -root -tree > /tmp/home/tree.txt 2>/dev/null
  head -n 60 /tmp/home/tree.txt
  pid=$(awk '/1024x27\+0\+0/{print $1; exit}' /tmp/home/tree.txt)
  cid=$(awk '/litebox chromium/{print $1; exit}' /tmp/home/tree.txt)
  echo "[wt] ids pid=[$pid] cid=[$cid]"
  if [ "$n" -gt 0 ]; then echo "XWD$n"; cat /tmp/home/root.xwd; echo; echo "END-XWD"; fi
  if [ -n "$pid" ]; then guard xwd -id "$pid" -silent -out /tmp/home/p.xwd; m=$(wc -c < /tmp/home/p.xwd 2>/dev/null || echo 0); echo "[wt] panel bytes=$m"; if [ "$m" -gt 0 ]; then echo "PXWD$m"; cat /tmp/home/p.xwd; echo; echo "END-PXWD"; fi; fi
  if [ -n "$cid" ]; then guard xwd -id "$cid" -silent -out /tmp/home/c.xwd; m=$(wc -c < /tmp/home/c.xwd 2>/dev/null || echo 0); echo "[wt] chromium bytes=$m"; if [ "$m" -gt 0 ]; then echo "CXWD$m"; cat /tmp/home/c.xwd; echo; echo "END-CXWD"; fi; fi
done
echo "[wt] done t=$t"
echo "[wt] session.log tail:"; head -c 6000 /tmp/home/session.log 2>/dev/null; echo
echo "[wt] xfwm4.log tail:"; head -c 3000 /tmp/home/xfwm4.log 2>/dev/null; echo
echo "[wt] xvfb.log tail:"; head -c 1500 /tmp/home/xvfb.log 2>/dev/null; echo
echo "[wt] chrome.err begin"; grep -a -v -E "Import Map|dbus|DBus|scheduler_loop|script_context|modulator|Created context|frame: |Num tracked|vaapi|libva|Desktop Identity|signin|token_service|idle_linux|CONSOLE" /tmp/home/chrome.err 2>/dev/null | tail -n 400 | head -c 90000; echo "[wt] chrome.err end"
