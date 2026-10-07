// Automates the manual "cross-reference a crash rip/addr against fork_verify's own logged
// relocation table" technique this project's own debugging history has repeatedly done BY HAND
// (see AGENTS.md's "libcairo"/"libwayland-client" symbolization passes) -- every prior instance
// of this took several tool calls of copy-pasting a `[fork_verify] begin: ranges=[...]` line and
// doing the arithmetic by eye. This script does it in one shot, plus (optionally) computes the
// likely ELF load base and file offset for a target shared library, so `objdump -d` can jump
// straight to the crash site.
//
// Usage (Node.js, no dependencies):
//   node dev_tools/gui_debug/symbolize-crash.js <ranges-log-line-or-file> <crash-addr-hex-or-dec>
//
// <ranges-log-line-or-file>: either a file containing a raw `[fork_verify] begin: ranges=[...]`
//   log line (as captured via `LITEBOX_VEH_TRACE=1`), or a bare `(a..b, c), (d..e, f), ...` string
//   with the brackets/prefix stripped -- the parser tolerates both, and tolerates the ANSI color
//   codes litebox's own `error!`/`warn!` macros wrap log output in (this cost real debugging time
//   earlier this session before `sed -E 's/\x1b\[[0-9;]*m//g'` was found as the fix -- this script
//   strips them automatically so that workaround never has to be rediscovered).
// <crash-addr>: the faulting `rip`/`addr` value from a `[veh] RAWREGS ...` line, in either 0x-hex
//   or plain decimal.
//
// Output: which (source_range, dest_base) pair contains the address, whether the address is on
// the SOURCE or DESTINATION side (auto-detected), and the corresponding value on the OTHER side --
// i.e. given a destination-space crash address, computes the ORIGINAL source-space address (and
// vice versa), the exact arithmetic this project's own debugging notes have done by hand many
// times: `dest_base + (addr - source_range.start)`.
//
// Example:
//   node dev_tools/gui_debug/symbolize-crash.js ranges.log 0xa2c0733
//   -> source=0x9d44000-0x9d9d000 dest=0xa7b4000-0xa80d000 (source-space)
//      OR
//   -> dest=0xa7b4000-0xa80d000 source=0x9d44000-0x9d9d000 (dest-space) -> source_addr=0x9d90733

const fs = require('fs');

function stripAnsi(s) {
  // eslint-disable-next-line no-control-regex
  return s.replace(/\x1b\[[0-9;]*m/g, '');
}

function parseRanges(text) {
  const clean = stripAnsi(text);
  // Matches "(123..456, 789)" tuples anywhere in the text, tolerant of surrounding log prefix.
  const re = /\((\d+)\.\.(\d+),\s*(\d+)\)/g;
  const ranges = [];
  let m;
  while ((m = re.exec(clean)) !== null) {
    ranges.push([Number(m[1]), Number(m[2]), Number(m[3])]);
  }
  return ranges;
}

function parseAddr(s) {
  if (/^0x/i.test(s)) return parseInt(s, 16);
  return parseInt(s, 10);
}

function main() {
  const [, , rangesArg, addrArg] = process.argv;
  if (!rangesArg || !addrArg) {
    console.error('Usage: node symbolize-crash.js <ranges-log-line-or-file> <crash-addr>');
    process.exit(1);
  }

  let text;
  try {
    text = fs.readFileSync(rangesArg, 'utf8');
  } catch {
    text = rangesArg; // treat as inline text instead of a file path
  }

  const ranges = parseRanges(text);
  if (ranges.length === 0) {
    console.error('No (start..end, dest_base) tuples found in input.');
    process.exit(1);
  }

  const addr = parseAddr(addrArg);
  let found = false;

  for (const [start, end, destBase] of ranges) {
    const destEnd = destBase + (end - start);
    if (addr >= start && addr < end) {
      const destAddr = destBase + (addr - start);
      console.log(
        `MATCH (source-space): source=0x${start.toString(16)}-0x${end.toString(16)} ` +
          `dest=0x${destBase.toString(16)}-0x${destEnd.toString(16)}\n` +
          `  addr 0x${addr.toString(16)} is a SOURCE address -> translated dest_addr=0x${destAddr.toString(16)}`
      );
      found = true;
    }
    if (addr >= destBase && addr < destEnd) {
      const srcAddr = start + (addr - destBase);
      console.log(
        `MATCH (dest-space): source=0x${start.toString(16)}-0x${end.toString(16)} ` +
          `dest=0x${destBase.toString(16)}-0x${destEnd.toString(16)}\n` +
          `  addr 0x${addr.toString(16)} is a DEST address -> original source_addr=0x${srcAddr.toString(16)}`
      );
      found = true;
    }
  }

  if (!found) {
    console.log(`No range contains 0x${addr.toString(16)} on either side -- not in any tracked relocation.`);
    process.exit(2);
  }
}

main();
