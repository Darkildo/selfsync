// Распаковка deflate без заголовка (RFC 1951) — для WASM и словарей, которые
// сборка кладёт в main.js сжатыми (src/build/pack.mjs). Своя, синхронная и без
// зависимостей: DecompressionStream асинхронный и есть в WKWebView только с
// iOS 16.4, а Obsidian поддерживает iOS 15.
//
// Декодер канонических кодов Хаффмана — табличный: первые FAST бит кода
// разбираются одним чтением, длинные коды дочитываются по битам.

const LEN_BASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/** Сколько бит кода разбирается таблицей за раз. */
const FAST = 9;

interface Huffman {
  /** Индекс — следующие FAST бит потока (младший бит первый): (длина << 9) | символ, 0 — код длиннее FAST. */
  fast: Uint16Array;
  /** Число кодов каждой длины и символы по возрастанию кода — для длинных кодов. */
  counts: Uint16Array;
  symbols: Uint16Array;
}

function huffman(lengths: ArrayLike<number>, n: number): Huffman {
  const counts = new Uint16Array(16);
  for (let i = 0; i < n; i++) counts[lengths[i]!]!++;
  counts[0] = 0;
  let left = 1;
  for (let len = 1; len < 16; len++) {
    left = (left << 1) - counts[len]!;
    if (left < 0) throw new Error("inflate: переполненный код Хаффмана");
  }
  const offs = new Uint16Array(16);
  for (let len = 1; len < 15; len++) offs[len + 1] = offs[len]! + counts[len]!;
  const symbols = new Uint16Array(n);
  for (let i = 0; i < n; i++) if (lengths[i]) symbols[offs[lengths[i]!]!++] = i;

  // Таблица быстрого разбора: канонический код, развёрнутый в порядок битов потока.
  const fast = new Uint16Array(1 << FAST);
  let code = 0;
  let index = 0;
  for (let len = 1; len <= FAST; len++) {
    for (let k = 0; k < counts[len]!; k++) {
      let rev = 0;
      for (let b = 0; b < len; b++) rev |= ((code >> b) & 1) << (len - 1 - b);
      const entry = (len << 9) | symbols[index + k]!;
      for (let fill = rev; fill < 1 << FAST; fill += 1 << len) fast[fill] = entry;
      code++;
    }
    index += counts[len]!;
    code <<= 1;
  }
  return { fast, counts, symbols };
}

let FIXED_LIT: Huffman | undefined;
let FIXED_DIST: Huffman | undefined;

function fixed(): [Huffman, Huffman] {
  if (!FIXED_LIT || !FIXED_DIST) {
    const l = new Uint8Array(288);
    l.fill(8, 0, 144);
    l.fill(9, 144, 256);
    l.fill(7, 256, 280);
    l.fill(8, 280, 288);
    FIXED_LIT = huffman(l, 288);
    FIXED_DIST = huffman(new Uint8Array(30).fill(5), 30);
  }
  return [FIXED_LIT, FIXED_DIST];
}

class Inflater {
  private pos = 0;
  private buf = 0;
  private cnt = 0;
  private out: Uint8Array;
  private len = 0;

  constructor(
    private readonly src: Uint8Array,
    sizeHint: number,
  ) {
    this.out = new Uint8Array(Math.max(sizeHint, src.length * 4, 1024));
  }

  /** Добрать в буфер не меньше need бит (за концом входа — нули, их ловит проверка ниже). */
  private fill(need: number): void {
    while (this.cnt < need) {
      if (this.pos > this.src.length + 4) throw new Error("inflate: поток оборван");
      this.buf |= (this.src[this.pos++] ?? 0) << this.cnt;
      this.cnt += 8;
    }
  }

  private bits(n: number): number {
    if (n === 0) return 0;
    this.fill(n);
    const v = this.buf & ((1 << n) - 1);
    this.buf >>>= n;
    this.cnt -= n;
    return v;
  }

  private decode(h: Huffman): number {
    this.fill(FAST);
    const e = h.fast[this.buf & ((1 << FAST) - 1)]!;
    if (e !== 0) {
      const l = e >> 9;
      this.buf >>>= l;
      this.cnt -= l;
      return e & 511;
    }
    // Код длиннее FAST бит: канонический разбор по одному биту.
    let code = 0;
    let first = 0;
    let index = 0;
    for (let len = 1; len < 16; len++) {
      code |= this.bits(1);
      const count = h.counts[len]!;
      if (code - first < count) return h.symbols[index + code - first]!;
      index += count;
      first = (first + count) << 1;
      code <<= 1;
    }
    throw new Error("inflate: неверный код Хаффмана");
  }

  private room(n: number): void {
    if (this.len + n <= this.out.length) return;
    let size = this.out.length * 2;
    while (size < this.len + n) size *= 2;
    const next = new Uint8Array(size);
    next.set(this.out.subarray(0, this.len));
    this.out = next;
  }

  private stored(): void {
    // Блок без сжатия начинается с границы байта: остаток текущего байта —
    // выравнивание, а целые байты, уже взятые в буфер, возвращаются во вход.
    this.pos -= this.cnt >> 3;
    this.buf = 0;
    this.cnt = 0;
    const s = this.src;
    if (this.pos + 4 > s.length) throw new Error("inflate: поток оборван");
    const n = s[this.pos]! | (s[this.pos + 1]! << 8);
    const nn = s[this.pos + 2]! | (s[this.pos + 3]! << 8);
    if ((n ^ 0xffff) !== nn) throw new Error("inflate: неверная длина блока");
    this.pos += 4;
    if (this.pos + n > s.length) throw new Error("inflate: поток оборван");
    this.room(n);
    this.out.set(s.subarray(this.pos, this.pos + n), this.len);
    this.len += n;
    this.pos += n;
  }

  private dynamic(): [Huffman, Huffman] {
    const nlen = this.bits(5) + 257;
    const ndist = this.bits(5) + 1;
    const ncode = this.bits(4) + 4;
    if (nlen > 286 || ndist > 30) throw new Error("inflate: неверный заголовок блока");
    const cl = new Uint8Array(19);
    for (let i = 0; i < ncode; i++) cl[CL_ORDER[i]!] = this.bits(3);
    const clh = huffman(cl, 19);
    const lengths = new Uint8Array(nlen + ndist);
    for (let i = 0; i < nlen + ndist; ) {
      const sym = this.decode(clh);
      if (sym < 16) {
        lengths[i++] = sym;
        continue;
      }
      let rep: number;
      let val = 0;
      if (sym === 16) {
        if (i === 0) throw new Error("inflate: повтор без предыдущей длины");
        val = lengths[i - 1]!;
        rep = 3 + this.bits(2);
      } else if (sym === 17) rep = 3 + this.bits(3);
      else rep = 11 + this.bits(7);
      if (i + rep > nlen + ndist) throw new Error("inflate: лишние длины кодов");
      lengths.fill(val, i, i + rep);
      i += rep;
    }
    return [huffman(lengths.subarray(0, nlen), nlen), huffman(lengths.subarray(nlen), ndist)];
  }

  private codes(lit: Huffman, dist: Huffman): void {
    for (;;) {
      const sym = this.decode(lit);
      if (sym < 256) {
        this.room(1);
        this.out[this.len++] = sym;
      } else if (sym === 256) {
        return;
      } else {
        const li = sym - 257;
        if (li >= 29) throw new Error("inflate: неверная длина");
        const n = LEN_BASE[li]! + this.bits(LEN_EXTRA[li]!);
        const di = this.decode(dist);
        if (di >= 30) throw new Error("inflate: неверное расстояние");
        const d = DIST_BASE[di]! + this.bits(DIST_EXTRA[di]!);
        if (d > this.len) throw new Error("inflate: расстояние за началом");
        this.room(n);
        const o = this.out;
        let from = this.len - d;
        if (d >= n) {
          o.copyWithin(this.len, from, from + n);
          this.len += n;
        } else {
          // Перекрывающийся повтор: байты копируются по одному, по мере записи.
          for (let k = 0; k < n; k++) o[this.len++] = o[from++]!;
        }
      }
    }
  }

  run(): Uint8Array {
    let last = 0;
    while (!last) {
      last = this.bits(1);
      const type = this.bits(2);
      if (type === 0) this.stored();
      else if (type === 1) this.codes(...fixed());
      else if (type === 2) this.codes(...this.dynamic());
      else throw new Error("inflate: неверный тип блока");
    }
    // Поток кончился раньше, чем данные (дочитаны нули за концом входа).
    if (this.pos - (this.cnt >> 3) > this.src.length) throw new Error("inflate: поток оборван");
    return this.out.slice(0, this.len);
  }
}

/** Распаковать deflate без заголовка; `sizeHint` — ожидаемый размер, если известен. */
export function inflateRaw(src: Uint8Array, sizeHint = 0): Uint8Array {
  return new Inflater(src, sizeHint).run();
}
