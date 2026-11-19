# Decode the 50-byte `68 32` status frames from the pitpat debug log.
# usage: journalctl -u pitpat --since "2026-11-17 12:04" -o short | awk -f frames.awk
function h(x,   i, n) { x = tolower(x); n = 0
  for (i = 1; i <= length(x); i++) n = n * 16 + index("0123456789abcdef", substr(x, i, 1)) - 1
  return n }
function b(k) { return h(f[s + k]) }
function u16(k) { return b(k) * 256 + b(k + 1) }
function u32(k) { return u16(k) * 65536 + u16(k + 2) }
{
  gsub(/\033\[[0-9;]*m/, "")            # drop ANSI colours, if any
  n = split($0, f, /[ =]+/)
  s = 0
  for (i = 1; i <= n; i++) if (f[i] == "bytes" && f[i + 1] == "68" && f[i + 2] == "32") { s = i + 1; break }
  if (!s) next
  t = $3
  if (t == last) next                   # one line per second
  last = t
  printf "%s state=%02x speed=%.1f dist=%d dur=%d kcal@18=%d steps@43=%d raw41=%d\n", t,
    b(26), u16(3) / 1000, u32(7), u32(20) / 1000, u16(18), u16(43), u16(41)
}
