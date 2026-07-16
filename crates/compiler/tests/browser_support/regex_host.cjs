'use strict';
// elm/regex host imports for the WasmGC backend, delegating to JS RegExp (the
// same way the trig kernels delegate to Math.*, for exact parity). One handle
// table of compiled RegExps; `find`/`split` write a length-prefixed blob into
// linear memory that the wasm side reads (byte offsets into the UTF-8 subject).
// `getMem` returns the (possibly re-grown) WebAssembly.Memory.
function regexHost(getMem) {
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const table = [];
  const mem = () => getMem();
  const readStr = (p, l) => dec.decode(new Uint8Array(mem().buffer, p, l));
  const ensure = (end) => {
    const cur = mem().buffer.byteLength;
    if (end > cur) getMem().grow(Math.ceil((end - cur) / 65536));
  };
  // byte offset (UTF-8) of a UTF-16 index into `s`
  const u16ToByte = (s, i) => Buffer.byteLength(s.slice(0, i), 'utf8');

  return {
    host_regex_compile: (pp, pl, flags) => {
      try {
        const re = new RegExp(readStr(pp, pl), 'g' + ((flags & 1) ? 'i' : '') + ((flags & 2) ? 'm' : ''));
        table.push(re);
        return table.length - 1;
      } catch (e) {
        return -1;
      }
    },
    host_regex_find: (id, sp, sl, n, out) => {
      if (id < 0) return 0;
      const s = readStr(sp, sl);
      const re = table[id];
      re.lastIndex = 0;
      let cur = out, count = 0, m, prev = -1;
      const wi = (v) => { ensure(cur + 4); new DataView(mem().buffer).setInt32(cur, v, true); cur += 4; };
      const ws = (str) => {
        const b = enc.encode(str);
        wi(b.length);
        ensure(cur + b.length);
        new Uint8Array(mem().buffer, cur, b.length).set(b);
        cur += b.length;
      };
      while (count < n && (m = re.exec(s))) {
        if (prev === re.lastIndex) { re.lastIndex++; continue; }
        prev = re.lastIndex;
        wi(u16ToByte(s, m.index)); // byteStart
        ws(m[0]); // match string
        wi(m.length - 1); // nsubs
        for (let i = 1; i < m.length; i++) {
          if (m[i] == null) wi(0);
          else { wi(1); ws(m[i]); }
        }
        count++;
      }
      return count;
    },
    host_regex_split: (id, sp, sl, n, out) => {
      const s = readStr(sp, sl);
      const parts = [];
      if (id < 0) {
        parts.push(s);
      } else {
        // Mirror elm/regex's _Parser_splitAtMost: n cuts, then the remainder.
        const re = table[id];
        re.lastIndex = 0;
        let start = 0, k = n, m;
        while (k-- > 0 && (m = re.exec(s))) {
          parts.push(s.slice(start, m.index));
          start = re.lastIndex;
          if (m.index === re.lastIndex) re.lastIndex++;
        }
        parts.push(s.slice(start));
      }
      let cur = out;
      for (const p of parts) {
        const b = enc.encode(p);
        ensure(cur + 4 + b.length);
        new DataView(mem().buffer).setInt32(cur, b.length, true);
        cur += 4;
        new Uint8Array(mem().buffer, cur, b.length).set(b);
        cur += b.length;
      }
      return parts.length;
    },
  };
}

module.exports = { regexHost };
