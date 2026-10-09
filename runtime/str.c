/* fpython runtime — strings and bytes (immutable, UTF-8). */
#include "fpy.h"
#include <ctype.h>
#include <stdarg.h>

static FpyStr *str_eq_dummy;
#define SMALL_INT_STRS 1024
static FpyStr *small_int_strs[SMALL_INT_STRS];
static int utf8_encode(uint32_t cp, char *out);
static uint32_t utf8_decode(const char *s, int64_t *len);
static FpyStr *single_chars[128];
static FpyStr *empty_str;

static int64_t count_chars(const char *s, int64_t n) {
    int64_t c = 0;
    for (int64_t i = 0; i < n; i++) c += ((uint8_t)s[i] & 0xC0) != 0x80;
    return c;
}

static FpyStr *str_alloc(int64_t len) {
    FpyStr *s = fpy_alloc(&fpy_type_str, sizeof(FpyStr) + (size_t)len + 1);
    s->len = len;
    return s;
}

FpyStr *fpy_str_new(const char *data, int64_t len) {
    if (len == 0 && empty_str) return empty_str;
    if (len == 1 && (uint8_t)data[0] < 128 && single_chars[(uint8_t)data[0]]) return single_chars[(uint8_t)data[0]];
    FpyStr *s = str_alloc(len);
    memcpy(s->data, data, (size_t)len);
    s->data[len] = 0;
    s->nchars = count_chars(data, len);
    return s;
}

FpyStr *fpy_str_lit(const char *data, int64_t len, int64_t nchars) {
    FpyStr *s = str_alloc(len);
    memcpy(s->data, data, (size_t)len);
    s->data[len] = 0;
    s->nchars = nchars;
    return s;
}

FpyStr *fpy_str_from_cstr(const char *s) { return fpy_str_new(s, (int64_t)strlen(s)); }

void fpy_str_init_cache(void) {
    for (int i = 0; i < 128; i++) {
        char c = (char)i;
        FpyStr *s = str_alloc(1);
        s->data[0] = c;
        s->data[1] = 0;
        s->nchars = 1;
        single_chars[i] = s;
    }
    empty_str = str_alloc(0);
    empty_str->data[0] = 0;
    (void)str_eq_dummy;
}

void fpy_str_mark_cache(void) {
    for (int i = 0; i < SMALL_INT_STRS; i++)
        if (small_int_strs[i]) fpy_gc_mark(small_int_strs[i]);
    for (int i = 0; i < 128; i++) fpy_gc_mark(single_chars[i]);
    fpy_gc_mark(empty_str);
}

FpyStr *fpy_str_concat(FpyStr *a, FpyStr *b) {
    if (a->len == 0) return b;
    if (b->len == 0) return a;
    FpyStr *s = str_alloc(a->len + b->len);
    memcpy(s->data, a->data, (size_t)a->len);
    memcpy(s->data + a->len, b->data, (size_t)b->len);
    s->data[s->len] = 0;
    s->nchars = a->nchars + b->nchars;
    return s;
}

FpyStr *fpy_str_repeat(FpyStr *a, fpy_int n) {
    if (n <= 0 || a->len == 0) return fpy_str_new("", 0);
    int64_t total;
    if (__builtin_mul_overflow(a->len, n, &total)) fpy_raise_code(FPY_EXC_MEMORY, "repeated string is too long");
    FpyStr *s = str_alloc(total);
    for (int64_t i = 0; i < n; i++) memcpy(s->data + i * a->len, a->data, (size_t)a->len);
    s->data[total] = 0;
    s->nchars = a->nchars * n;
    return s;
}

bool fpy_str_eq(FpyStr *a, FpyStr *b) {
    if (a == b) return true;
    if (!a || !b) return false;
    if (a->len != b->len) return false;
    if (a->hash && b->hash && a->hash != b->hash) return false;
    return memcmp(a->data, b->data, (size_t)a->len) == 0;
}

int fpy_str_cmp(FpyStr *a, FpyStr *b) {
    int64_t n = a->len < b->len ? a->len : b->len;
    int c = memcmp(a->data, b->data, (size_t)n);
    if (c) return c < 0 ? -1 : 1;
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}

int64_t fpy_str_hash(FpyStr *s) {
    if (s->hash) return s->hash;
    /* FNV-1a 64 */
    uint64_t h = 1469598103934665603ull;
    for (int64_t i = 0; i < s->len; i++) {
        h ^= (uint8_t)s->data[i];
        h *= 1099511628211ull;
    }
    int64_t r = (int64_t)h;
    if (r == 0 || r == FPY_DELETED_HASH || r == -1) r = 1;
    s->hash = r;
    return r;
}

/* byte offset of code point index i (0 <= i <= nchars) */
static int64_t cp_offset(FpyStr *s, int64_t i) {
    if (s->len == s->nchars) return i;
    int64_t off = 0;
    while (i > 0 && off < s->len) {
        off++;
        while (off < s->len && ((uint8_t)s->data[off] & 0xC0) == 0x80) off++;
        i--;
    }
    return off;
}

static int64_t cp_len_at(FpyStr *s, int64_t off) {
    int64_t e = off + 1;
    while (e < s->len && ((uint8_t)s->data[e] & 0xC0) == 0x80) e++;
    return e - off;
}

FpyStr *fpy_str_getitem(FpyStr *s, fpy_int i) {
    i = fpy_norm_index(i, s->nchars, "string");
    if (s->len == s->nchars) {
        uint8_t c = (uint8_t)s->data[i];
        if (c < 128) return single_chars[c];
        return fpy_str_new(s->data + i, 1);
    }
    int64_t off = cp_offset(s, i);
    return fpy_str_new(s->data + off, cp_len_at(s, off));
}

static void slice_indices(int64_t len, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step,
                          int64_t *start, int64_t *stop) {
    if (step == 0) fpy_raise_code(FPY_EXC_VALUE, "slice step cannot be zero");
    if (step > 0) {
        if (!has_lo) lo = 0;
        else if (lo < 0) { lo += len; if (lo < 0) lo = 0; }
        else if (lo > len) lo = len;
        if (!has_hi) hi = len;
        else if (hi < 0) { hi += len; if (hi < 0) hi = 0; }
        else if (hi > len) hi = len;
    } else {
        if (!has_lo) lo = len - 1;
        else if (lo < 0) { lo += len; if (lo < 0) lo = -1; }
        else if (lo >= len) lo = len - 1;
        if (!has_hi) hi = -1;
        else if (hi < 0) { hi += len; if (hi < 0) hi = -1; }
        else if (hi >= len) hi = len - 1;
    }
    *start = lo;
    *stop = hi;
}

int64_t fpy_slice_len(int64_t start, int64_t stop, int64_t step) {
    if (step > 0) return stop > start ? (stop - start + step - 1) / step : 0;
    return start > stop ? (start - stop - step - 1) / (-step) : 0;
}

void fpy_slice_indices(int64_t len, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step,
                       int64_t *start, int64_t *stop) {
    slice_indices(len, has_lo, lo, has_hi, hi, step, start, stop);
}

FpyStr *fpy_str_slice(FpyStr *s, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step) {
    int64_t start, stop;
    slice_indices(s->nchars, has_lo, lo, has_hi, hi, step, &start, &stop);
    int64_t n = fpy_slice_len(start, stop, step);
    if (step == 1) {
        if (n <= 0) return fpy_str_new("", 0);
        int64_t a = cp_offset(s, start), b = cp_offset(s, stop);
        return fpy_str_new(s->data + a, b - a);
    }
    FpySB sb;
    fpy_sb_init(&sb);
    for (int64_t k = 0, i = start; k < n; k++, i += step) {
        int64_t off = cp_offset(s, i);
        fpy_sb_add(&sb, s->data + off, cp_len_at(s, off));
    }
    return fpy_sb_finish(&sb);
}

static const char *find_bytes(const char *h, int64_t hn, const char *n, int64_t nn) {
    if (nn == 0) return h;
    if (nn > hn) return NULL;
    if (nn == 1) return memchr(h, n[0], (size_t)hn);
    const char *end = h + hn - nn;
    for (const char *p = h; p <= end; p++) {
        p = memchr(p, n[0], (size_t)(end - p + 1));
        if (!p) return NULL;
        if (memcmp(p, n, (size_t)nn) == 0) return p;
    }
    return NULL;
}

bool fpy_str_contains(FpyStr *hay, FpyStr *needle) {
    return find_bytes(hay->data, hay->len, needle->data, needle->len) != NULL;
}

FpyStr *fpy_int_to_str(fpy_int v) {
    if (v >= 0 && v < SMALL_INT_STRS && small_int_strs[v]) return small_int_strs[v];
    char buf[24];
    char *p = buf + sizeof buf;
    uint64_t u = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
    do {
        *--p = (char)('0' + u % 10);
        u /= 10;
    } while (u);
    if (v < 0) *--p = '-';
    int n = (int)(buf + sizeof buf - p);
    FpyStr *s = fpy_str_lit(p, n, n);
    if (v >= 0 && v < SMALL_INT_STRS) small_int_strs[v] = s;
    return s;
}

static int fmt_repr_double(char *buf, size_t cap, double v) {
    if (isnan(v)) return snprintf(buf, cap, "nan");
    if (isinf(v)) return snprintf(buf, cap, v > 0 ? "inf" : "-inf");
    if (v == 0.0) return snprintf(buf, cap, signbit(v) ? "-0.0" : "0.0");
    /* shortest round-trip representation, Python style */
    int n = 0;
    for (int prec = 1; prec <= 17; prec++) {
        n = snprintf(buf, cap, "%.*g", prec, v);
        if (strtod(buf, NULL) == v) break;
    }
    double av = fabs(v);
    bool sci = av >= 1e16 || av < 1e-4;
    if (!sci) {
        if (strchr(buf, 'e')) {
            n = snprintf(buf, cap, "%.17f", v);
            /* trim trailing zeros, keep one */
            while (n > 0 && buf[n - 1] == '0') n--;
            if (buf[n - 1] == '.') n++;
            buf[n] = 0;
            /* redo minimal: find shortest */
            for (int prec = 0; prec <= 17; prec++) {
                char t[64];
                int m = snprintf(t, sizeof t, "%.*f", prec, v);
                if (strtod(t, NULL) == v) { memcpy(buf, t, (size_t)m + 1); n = m; break; }
            }
        }
        if (!strchr(buf, '.') && !strchr(buf, 'n') && !strchr(buf, 'e')) { buf[n++] = '.'; buf[n++] = '0'; buf[n] = 0; }
        return n;
    }
    /* scientific: Python prints e.g. 1e+16, 1.5e-05 */
    char *e = strchr(buf, 'e');
    if (!e) {
        for (int prec = 1; prec <= 17; prec++) {
            n = snprintf(buf, cap, "%.*e", prec - 1, v);
            if (strtod(buf, NULL) == v) break;
        }
        e = strchr(buf, 'e');
        /* strip trailing zeros in mantissa */
        char *m = e;
        while (m > buf && m[-1] == '0') m--;
        if (m > buf && m[-1] == '.') m--;
        memmove(m, e, strlen(e) + 1);
        n = (int)strlen(buf);
        e = strchr(buf, 'e');
    }
    /* normalize exponent to at least 2 digits */
    char sign = e[1];
    char *digits = e + 2;
    if (sign != '+' && sign != '-') { sign = '+'; digits = e + 1; }
    while (*digits == '0' && digits[1]) digits++;
    char tail[16];
    snprintf(tail, sizeof tail, "e%c%s%s", sign, strlen(digits) < 2 ? "0" : "", digits);
    strcpy(e, tail);
    return (int)strlen(buf);
}

FpyStr *fpy_float_to_str(double v) {
    char buf[64];
    int n = fmt_repr_double(buf, sizeof buf, v);
    return fpy_str_lit(buf, n, n);
}

FpyStr *fpy_bool_to_str(bool b) { return b ? fpy_str_lit("True", 4, 4) : fpy_str_lit("False", 5, 5); }

/* ascii(): escape every non-ASCII code point of a repr() as \xNN, \uNNNN or \UNNNNNNNN. */
FpyStr *fpy_ascii_escape(FpyStr *s) {
    FpySB sb;
    fpy_sb_init(&sb);
    const unsigned char *p = (const unsigned char *)s->data, *end = p + s->len;
    while (p < end) {
        if (*p < 0x80) { fpy_sb_add(&sb, (const char *)p, 1); p++; continue; }
        uint32_t cp; int n;
        if ((*p & 0xE0) == 0xC0) { cp = *p & 0x1F; n = 1; }
        else if ((*p & 0xF0) == 0xE0) { cp = *p & 0x0F; n = 2; }
        else { cp = *p & 0x07; n = 3; }
        p++;
        for (int i = 0; i < n && p < end; i++, p++) cp = (cp << 6) | (*p & 0x3F);
        char tmp[16];
        int k = cp <= 0xFF ? snprintf(tmp, sizeof tmp, "\\x%02x", cp) : cp <= 0xFFFF ? snprintf(tmp, sizeof tmp, "\\u%04x", cp) : snprintf(tmp, sizeof tmp, "\\U%08x", cp);
        fpy_sb_add(&sb, tmp, k);
    }
    return fpy_sb_finish(&sb);
}

FpyStr *fpy_str_repr(FpyStr *s) {
    bool has_sq = memchr(s->data, '\'', (size_t)s->len) != NULL;
    bool has_dq = memchr(s->data, '"', (size_t)s->len) != NULL;
    char q = (has_sq && !has_dq) ? '"' : '\'';
    FpySB sb;
    fpy_sb_init(&sb);
    fpy_sb_add(&sb, &q, 1);
    for (int64_t i = 0; i < s->len; i++) {
        uint8_t c = (uint8_t)s->data[i];
        char tmp[8];
        if (c == (uint8_t)q || c == '\\') { tmp[0] = '\\'; tmp[1] = (char)c; fpy_sb_add(&sb, tmp, 2); }
        else if (c == '\n') fpy_sb_add(&sb, "\\n", 2);
        else if (c == '\r') fpy_sb_add(&sb, "\\r", 2);
        else if (c == '\t') fpy_sb_add(&sb, "\\t", 2);
        else if (c < 32 || c == 127) { int n = snprintf(tmp, sizeof tmp, "\\x%02x", c); fpy_sb_add(&sb, tmp, n); }
        else fpy_sb_add(&sb, (char *)&c, 1);
    }
    fpy_sb_add(&sb, &q, 1);
    return fpy_sb_finish(&sb);
}

static int utf8_encode(uint32_t cp, char *out) {
    if (cp < 0x80) { out[0] = (char)cp; return 1; }
    if (cp < 0x800) { out[0] = (char)(0xC0 | (cp >> 6)); out[1] = (char)(0x80 | (cp & 0x3F)); return 2; }
    if (cp < 0x10000) {
        out[0] = (char)(0xE0 | (cp >> 12)); out[1] = (char)(0x80 | ((cp >> 6) & 0x3F)); out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18)); out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F)); out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

static uint32_t utf8_decode(const char *s, int64_t *len) {
    uint8_t c = (uint8_t)s[0];
    if (c < 0x80) { *len = 1; return c; }
    if ((c & 0xE0) == 0xC0) { *len = 2; return ((c & 0x1Fu) << 6) | (s[1] & 0x3F); }
    if ((c & 0xF0) == 0xE0) { *len = 3; return ((c & 0x0Fu) << 12) | ((s[1] & 0x3Fu) << 6) | (s[2] & 0x3F); }
    *len = 4;
    return ((c & 0x07u) << 18) | ((s[1] & 0x3Fu) << 12) | ((s[2] & 0x3Fu) << 6) | (s[3] & 0x3F);
}

FpyStr *fpy_str_char(uint32_t cp) {
    if (cp < 128) return single_chars[cp];
    char buf[4];
    int n = utf8_encode(cp, buf);
    return fpy_str_lit(buf, n, 1);
}

FpyStr *fpy_chr(fpy_int cp) {
    if (cp < 0 || cp > 0x10FFFF) fpy_raise_code(FPY_EXC_VALUE, "chr() arg not in range(0x110000)");
    return fpy_str_char((uint32_t)cp);
}

fpy_int fpy_str_ord(FpyStr *s) {
    if (s->nchars != 1) fpy_raise_fmt(FPY_EXC_TYPE, "ord() expected a character, but string of length %lld found", (long long)s->nchars);
    int64_t n;
    return utf8_decode(s->data, &n);
}

fpy_int fpy_str_to_int(FpyStr *s, fpy_int base) {
    const char *p = s->data;
    while (*p && isspace((unsigned char)*p)) p++;
    int neg = 0;
    if (*p == '+' || *p == '-') neg = *p++ == '-';
    if (base == 0) {
        base = 10;
        if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) { base = 16; p += 2; }
        else if (p[0] == '0' && (p[1] == 'o' || p[1] == 'O')) { base = 8; p += 2; }
        else if (p[0] == '0' && (p[1] == 'b' || p[1] == 'B')) { base = 2; p += 2; }
    } else if (base == 16 && p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) p += 2;
    uint64_t v = 0;
    int digits = 0;
    for (; *p; p++) {
        if (*p == '_' && digits) continue;
        int d;
        if (*p >= '0' && *p <= '9') d = *p - '0';
        else if (*p >= 'a' && *p <= 'z') d = *p - 'a' + 10;
        else if (*p >= 'A' && *p <= 'Z') d = *p - 'A' + 10;
        else break;
        if (d >= base) break;
        if (v > (UINT64_MAX - (uint64_t)d) / (uint64_t)base) fpy_overflow();
        v = v * (uint64_t)base + (uint64_t)d;
        digits++;
    }
    while (*p && isspace((unsigned char)*p)) p++;
    if (!digits || *p) {
        FpyStr *r = fpy_str_repr(s);
        fpy_raise_fmt(FPY_EXC_VALUE, "invalid literal for int() with base %lld: %s", (long long)base, r->data);
    }
    if (neg) {
        if (v > (uint64_t)INT64_MAX + 1) fpy_overflow();
        return (fpy_int)(0 - v);
    }
    if (v > (uint64_t)INT64_MAX) fpy_overflow();
    return (fpy_int)v;
}

double fpy_str_to_float(FpyStr *s) {
    const char *p = s->data;
    while (*p && isspace((unsigned char)*p)) p++;
    char *end;
    double v = strtod(p, &end);
    if (end == p) goto bad;
    while (*end && isspace((unsigned char)*end)) end++;
    if (*end) goto bad;
    return v;
bad:;
    FpyStr *r = fpy_str_repr(s);
    fpy_raise_fmt(FPY_EXC_VALUE, "could not convert string to float: %s", r->data);
}

/* ------------------------------------------------------------------ builder */
void fpy_sb_init(FpySB *sb) {
    sb->cap = 64;
    sb->buf = malloc((size_t)sb->cap);
    sb->len = 0;
    sb->nchars = -1;
}
void fpy_sb_add(FpySB *sb, const char *s, int64_t n) {
    if (sb->len + n + 1 > sb->cap) {
        while (sb->len + n + 1 > sb->cap) sb->cap *= 2;
        sb->buf = realloc(sb->buf, (size_t)sb->cap);
    }
    memcpy(sb->buf + sb->len, s, (size_t)n);
    sb->len += n;
}
void fpy_sb_add_str(FpySB *sb, FpyStr *s) { fpy_sb_add(sb, s->data, s->len); }
void fpy_sb_add_cstr(FpySB *sb, const char *s) { fpy_sb_add(sb, s, (int64_t)strlen(s)); }
FpyStr *fpy_sb_finish(FpySB *sb) {
    FpyStr *s = fpy_str_new(sb->buf, sb->len);
    free(sb->buf);
    sb->buf = NULL;
    return s;
}

/* ------------------------------------------------------------------ format specs */
typedef struct Spec {
    char fill, align, sign;
    bool alt, zero, has_prec;
    int width, prec;
    char grouping, type;
} Spec;

static Spec parse_spec(FpyStr *spec) {
    Spec s = {' ', 0, '-', false, false, false, -1, -1, 0, 0};
    if (!spec || spec->len == 0) return s;
    const char *p = spec->data;
    if (p[0] && p[1] && strchr("<>=^", p[1])) { s.fill = p[0]; s.align = p[1]; p += 2; }
    else if (p[0] && strchr("<>=^", p[0])) { s.align = p[0]; p++; }
    if (*p && strchr("+- ", *p)) s.sign = *p++;
    if (*p == '#') { s.alt = true; p++; }
    if (*p == '0') { s.zero = true; p++; }
    if (isdigit((unsigned char)*p)) { s.width = 0; while (isdigit((unsigned char)*p)) s.width = s.width * 10 + (*p++ - '0'); }
    if (*p == ',' || *p == '_') s.grouping = *p++;
    if (*p == '.') { p++; s.has_prec = true; s.prec = 0; while (isdigit((unsigned char)*p)) s.prec = s.prec * 10 + (*p++ - '0'); }
    if (*p) s.type = *p++;
    if (*p) fpy_raise_fmt(FPY_EXC_VALUE, "Invalid format specifier '%s'", spec->data);
    return s;
}

static FpyStr *pad(const char *body, int64_t blen, int64_t nchars, Spec *s, char default_align, int sign_len) {
    int64_t w = s->width;
    if (w <= nchars) return fpy_str_new(body, blen);
    int64_t padn = w - nchars;
    char align = s->align ? s->align : default_align;
    char fill = s->fill;
    if (s->zero && !s->align) { align = '='; fill = '0'; }
    FpySB sb;
    fpy_sb_init(&sb);
    int64_t left = 0, right = 0;
    if (align == '<') right = padn;
    else if (align == '>') left = padn;
    else if (align == '^') { left = padn / 2; right = padn - left; }
    else if (align == '=') {
        fpy_sb_add(&sb, body, sign_len);
        for (int64_t i = 0; i < padn; i++) fpy_sb_add(&sb, &fill, 1);
        fpy_sb_add(&sb, body + sign_len, blen - sign_len);
        return fpy_sb_finish(&sb);
    }
    for (int64_t i = 0; i < left; i++) fpy_sb_add(&sb, &fill, 1);
    fpy_sb_add(&sb, body, blen);
    for (int64_t i = 0; i < right; i++) fpy_sb_add(&sb, &fill, 1);
    return fpy_sb_finish(&sb);
}

static int group_digits(char *out, const char *digits, int n, char sep) {
    int o = 0;
    for (int i = 0; i < n; i++) {
        if (i > 0 && (n - i) % 3 == 0) out[o++] = sep;
        out[o++] = digits[i];
    }
    out[o] = 0;
    return o;
}

FpyStr *fpy_format_float(double v, FpyStr *spec);

FpyStr *fpy_format_int(fpy_int v, FpyStr *specs) {
    Spec s = parse_spec(specs);
    if (s.type && strchr("eEfFgG%", s.type)) return fpy_format_float((double)v, specs);
    char digits[80];
    uint64_t u = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
    int n;
    const char *prefix = "";
    switch (s.type) {
    case 'x': n = snprintf(digits, sizeof digits, "%llx", (unsigned long long)u); prefix = s.alt ? "0x" : ""; break;
    case 'X': n = snprintf(digits, sizeof digits, "%llX", (unsigned long long)u); prefix = s.alt ? "0X" : ""; break;
    case 'o': n = snprintf(digits, sizeof digits, "%llo", (unsigned long long)u); prefix = s.alt ? "0o" : ""; break;
    case 'b': {
        n = 0;
        char tmp[70];
        do { tmp[n++] = (char)('0' + (u & 1)); u >>= 1; } while (u);
        for (int i = 0; i < n; i++) digits[i] = tmp[n - 1 - i];
        digits[n] = 0;
        prefix = s.alt ? "0b" : "";
        break;
    }
    case 'c': {
        FpyStr *c = fpy_chr(v);
        return pad(c->data, c->len, 1, &s, '<', 0);
    }
    default: n = snprintf(digits, sizeof digits, "%llu", (unsigned long long)u);
    }
    char grouped[120];
    if (s.grouping) n = group_digits(grouped, digits, n, s.grouping);
    else memcpy(grouped, digits, (size_t)n + 1);
    char body[160];
    const char *sign = v < 0 ? "-" : (s.sign == '+' ? "+" : (s.sign == ' ' ? " " : ""));
    int bl = snprintf(body, sizeof body, "%s%s%s", sign, prefix, grouped);
    return pad(body, bl, bl, &s, '>', (int)(strlen(sign) + strlen(prefix)));
}

FpyStr *fpy_format_float(double v, FpyStr *specs) {
    Spec s = parse_spec(specs);
    char body[512];
    char num[400];
    double av = fabs(v);
    int n;
    char t = s.type;
    bool pct = false;
    if (t == '%') { av *= 100; t = 'f'; pct = true; }
    int prec = s.has_prec ? s.prec : 6;
    if (isnan(v)) n = snprintf(num, sizeof num, "nan");
    else if (isinf(v)) n = snprintf(num, sizeof num, "inf");
    else switch (t) {
    case 'f': case 'F': n = snprintf(num, sizeof num, "%.*f", prec, av); break;
    case 'e': case 'E': {
        n = snprintf(num, sizeof num, t == 'e' ? "%.*e" : "%.*E", prec, av);
        break;
    }
    case 'g': case 'G': n = snprintf(num, sizeof num, t == 'g' ? "%.*g" : "%.*G", prec == 0 ? 1 : prec, av); break;
    case 0:
        if (s.has_prec) {
            n = snprintf(num, sizeof num, "%.*g", prec == 0 ? 1 : prec, av);
            if (!strchr(num, '.') && !strchr(num, 'e') && !strchr(num, 'n') && !strchr(num, 'i') && av < 1e16) { /* Python keeps no .0 here */ }
        } else {
            n = fmt_repr_double(num, sizeof num, av);
        }
        break;
    default: fpy_raise_fmt(FPY_EXC_VALUE, "Unknown format code '%c' for object of type 'float'", t);
    }
    if (s.grouping) {
        char *dot = strchr(num, '.');
        int ilen = dot ? (int)(dot - num) : n;
        char g[440];
        int gl = group_digits(g, num, ilen, s.grouping);
        snprintf(g + gl, sizeof g - (size_t)gl, "%s", num + ilen);
        n = snprintf(num, sizeof num, "%s", g);
    }
    if (pct) { num[n++] = '%'; num[n] = 0; }
    bool neg = signbit(v) && !isnan(v);
    const char *sign = neg ? "-" : (s.sign == '+' ? "+" : (s.sign == ' ' ? " " : ""));
    int bl = snprintf(body, sizeof body, "%s%s", sign, num);
    return pad(body, bl, bl, &s, '>', (int)strlen(sign));
}

FpyStr *fpy_format_str(FpyStr *v, FpyStr *specs) {
    Spec s = parse_spec(specs);
    if (s.type && s.type != 's') fpy_raise_fmt(FPY_EXC_VALUE, "Unknown format code '%c' for object of type 'str'", s.type);
    if (s.has_prec && s.prec < v->nchars) v = fpy_str_slice(v, true, 0, true, s.prec, 1);
    return pad(v->data, v->len, v->nchars, &s, '<', 0);
}

/* ------------------------------------------------------------------ methods */
/* Simple Unicode case mapping covering Latin-1, Latin Extended-A, Greek and Cyrillic. */
static uint32_t cp_upper(uint32_t c) {
    if (c < 128) return (c >= 'a' && c <= 'z') ? c - 32 : c;
    if ((c >= 0xE0 && c <= 0xFE && c != 0xF7)) return c - 0x20;
    if (c == 0xFF) return 0x178;
    if (c >= 0x100 && c <= 0x17F) {
        if ((c >= 0x139 && c <= 0x148) || (c >= 0x179 && c <= 0x17E)) return (c & 1) ? c : c - 1;
        return (c & 1) ? c - 1 : c;
    }
    if (c >= 0x3B1 && c <= 0x3C9 && c != 0x3C2) return c - 0x20;
    if (c >= 0x430 && c <= 0x44F) return c - 0x20;
    if (c >= 0x450 && c <= 0x45F) return c - 0x50;
    return c;
}
static uint32_t cp_lower(uint32_t c) {
    if (c < 128) return (c >= 'A' && c <= 'Z') ? c + 32 : c;
    if ((c >= 0xC0 && c <= 0xDE && c != 0xD7)) return c + 0x20;
    if (c == 0x178) return 0xFF;
    if (c >= 0x100 && c <= 0x17F) {
        if ((c >= 0x139 && c <= 0x148) || (c >= 0x179 && c <= 0x17E)) return (c & 1) ? c + 1 : c;
        return (c & 1) ? c : c + 1;
    }
    if (c >= 0x391 && c <= 0x3A9) return c + 0x20;
    if (c >= 0x410 && c <= 0x42F) return c + 0x20;
    if (c >= 0x400 && c <= 0x40F) return c + 0x50;
    return c;
}
static FpyStr *map_case(FpyStr *s, int mode) { /* 0 upper, 1 lower, 2 swap */
    if (s->len == s->nchars) {
        FpyStr *r = fpy_str_lit(s->data, s->len, s->nchars);
        for (int64_t i = 0; i < r->len; i++) {
            unsigned char c = (unsigned char)r->data[i];
            r->data[i] = (char)(mode == 0 ? toupper(c) : mode == 1 ? tolower(c) : (isupper(c) ? tolower(c) : toupper(c)));
        }
        return r;
    }
    FpySB sb;
    fpy_sb_init(&sb);
    for (int64_t i = 0; i < s->len;) {
        int64_t n;
        uint32_t c = utf8_decode(s->data + i, &n);
        uint32_t m = mode == 0 ? cp_upper(c) : mode == 1 ? cp_lower(c) : (cp_upper(c) != c ? cp_upper(c) : cp_lower(c));
        char buf[4];
        fpy_sb_add(&sb, buf, utf8_encode(m, buf));
        i += n;
    }
    return fpy_sb_finish(&sb);
}
FpyStr *fpy_str_upper(FpyStr *s) { return map_case(s, 0); }
FpyStr *fpy_str_lower(FpyStr *s) { return map_case(s, 1); }
FpyStr *fpy_str_swapcase(FpyStr *s) { return map_case(s, 2); }
FpyStr *fpy_str_capitalize(FpyStr *s) {
    FpyStr *r = fpy_str_lower(s);
    if (r->len) r->data[0] = (char)toupper((unsigned char)r->data[0]);
    return r;
}
FpyStr *fpy_str_title(FpyStr *s) {
    FpyStr *r = fpy_str_lit(s->data, s->len, s->nchars);
    bool prev_alpha = false;
    for (int64_t i = 0; i < r->len; i++) {
        unsigned char c = (unsigned char)r->data[i];
        r->data[i] = (char)(prev_alpha ? tolower(c) : toupper(c));
        prev_alpha = isalpha(c);
    }
    return r;
}

static bool in_chars(FpyStr *chars, const char *p, int64_t n) {
    if (!chars) return n == 1 && isspace((unsigned char)*p);
    for (int64_t i = 0; i < chars->len;) {
        int64_t l = cp_len_at(chars, i);
        if (l == n && memcmp(chars->data + i, p, (size_t)n) == 0) return true;
        i += l;
    }
    return false;
}

static FpyStr *strip_impl(FpyStr *s, FpyStr *chars, bool left, bool right) {
    int64_t a = 0, b = s->len;
    if (left)
        while (a < b) { int64_t l = cp_len_at(s, a); if (!in_chars(chars, s->data + a, l)) break; a += l; }
    if (right)
        while (b > a) {
            int64_t st = b - 1;
            while (st > a && ((uint8_t)s->data[st] & 0xC0) == 0x80) st--;
            if (!in_chars(chars, s->data + st, b - st)) break;
            b = st;
        }
    if (a == 0 && b == s->len) return s;
    return fpy_str_new(s->data + a, b - a);
}
FpyStr *fpy_str_strip(FpyStr *s, FpyStr *chars) { return strip_impl(s, chars, true, true); }
FpyStr *fpy_str_lstrip(FpyStr *s, FpyStr *chars) { return strip_impl(s, chars, true, false); }
FpyStr *fpy_str_rstrip(FpyStr *s, FpyStr *chars) { return strip_impl(s, chars, false, true); }

FpyList *fpy_str_split(FpyStr *s, FpyStr *sep, fpy_int maxsplit) {
    FpyList *out = fpy_list_new(FK_OBJ, 4);
    if (!sep) {
        int64_t i = 0, n = s->len;
        while (i < n) {
            while (i < n && isspace((unsigned char)s->data[i])) i++;
            if (i >= n) break;
            if (maxsplit >= 0 && out->len >= maxsplit) {
                int64_t e = n;
                while (e > i && isspace((unsigned char)s->data[e - 1])) e--;
                fpy_list_append(out, fpy_wp(fpy_str_new(s->data + i, e - i)));
                break;
            }
            int64_t st = i;
            while (i < n && !isspace((unsigned char)s->data[i])) i++;
            fpy_list_append(out, fpy_wp(fpy_str_new(s->data + st, i - st)));
        }
        return out;
    }
    if (sep->len == 0) fpy_raise_code(FPY_EXC_VALUE, "empty separator");
    const char *p = s->data, *end = s->data + s->len;
    while (maxsplit < 0 || out->len < maxsplit) {
        const char *f = find_bytes(p, end - p, sep->data, sep->len);
        if (!f) break;
        fpy_list_append(out, fpy_wp(fpy_str_new(p, f - p)));
        p = f + sep->len;
    }
    fpy_list_append(out, fpy_wp(fpy_str_new(p, end - p)));
    return out;
}

FpyList *fpy_str_rsplit(FpyStr *s, FpyStr *sep, fpy_int maxsplit) {
    if (maxsplit < 0) return fpy_str_split(s, sep, maxsplit);
    FpyList *out = fpy_list_new(FK_OBJ, 4);
    int64_t end = s->len;
    if (!sep) {
        while (end > 0) {
            while (end > 0 && isspace((unsigned char)s->data[end - 1])) end--;
            if (end == 0) break;
            if (out->len >= maxsplit) {
                int64_t st = 0;
                while (st < end && isspace((unsigned char)s->data[st])) st++;
                fpy_list_append(out, fpy_wp(fpy_str_new(s->data + st, end - st)));
                end = 0;
                break;
            }
            int64_t st = end;
            while (st > 0 && !isspace((unsigned char)s->data[st - 1])) st--;
            fpy_list_append(out, fpy_wp(fpy_str_new(s->data + st, end - st)));
            end = st;
        }
    } else {
        while (out->len < maxsplit) {
            int64_t found = -1;
            for (int64_t i = end - sep->len; i >= 0; i--)
                if (memcmp(s->data + i, sep->data, (size_t)sep->len) == 0) { found = i; break; }
            if (found < 0) break;
            fpy_list_append(out, fpy_wp(fpy_str_new(s->data + found + sep->len, end - found - sep->len)));
            end = found;
        }
        fpy_list_append(out, fpy_wp(fpy_str_new(s->data, end)));
    }
    fpy_list_reverse(out);
    return out;
}

FpyList *fpy_str_splitlines(FpyStr *s, bool keepends) {
    FpyList *out = fpy_list_new(FK_OBJ, 4);
    int64_t i = 0, st = 0, n = s->len;
    while (i < n) {
        char c = s->data[i];
        if (c == '\n' || c == '\r') {
            int64_t e = i;
            if (c == '\r' && i + 1 < n && s->data[i + 1] == '\n') i++;
            i++;
            fpy_list_append(out, fpy_wp(fpy_str_new(s->data + st, (keepends ? i : e) - st)));
            st = i;
        } else i++;
    }
    if (st < n) fpy_list_append(out, fpy_wp(fpy_str_new(s->data + st, n - st)));
    return out;
}

FpyStr *fpy_str_join(FpyStr *sep, FpyList *items) {
    if (items->len == 0) return fpy_str_new("", 0);
    int64_t total = sep->len * (items->len - 1), nch = sep->nchars * (items->len - 1);
    for (int64_t i = 0; i < items->len; i++) {
        FpyStr *x = fpy_pw(items->items[i]);
        if (!x || x->ty != &fpy_type_str) fpy_raise_fmt(FPY_EXC_TYPE, "sequence item %lld: expected str instance", (long long)i);
        total += x->len;
        nch += x->nchars;
    }
    FpyStr *r = str_alloc(total);
    char *p = r->data;
    for (int64_t i = 0; i < items->len; i++) {
        if (i) { memcpy(p, sep->data, (size_t)sep->len); p += sep->len; }
        FpyStr *x = fpy_pw(items->items[i]);
        memcpy(p, x->data, (size_t)x->len);
        p += x->len;
    }
    *p = 0;
    r->nchars = nch;
    return r;
}

FpyStr *fpy_str_replace(FpyStr *s, FpyStr *old, FpyStr *new_, fpy_int count) {
    FpySB sb;
    fpy_sb_init(&sb);
    const char *p = s->data, *end = s->data + s->len;
    fpy_int done = 0;
    if (old->len == 0) {
        /* insert new_ between every character */
        for (int64_t i = 0; i <= s->len && (count < 0 || done < count);) {
            fpy_sb_add_str(&sb, new_);
            done++;
            if (i == s->len) { i++; p = end; break; }
            int64_t l = cp_len_at(s, i);
            fpy_sb_add(&sb, s->data + i, l);
            i += l;
            p = s->data + i;
        }
        if (p < end) fpy_sb_add(&sb, p, end - p);
        return fpy_sb_finish(&sb);
    }
    while (count < 0 || done < count) {
        const char *f = find_bytes(p, end - p, old->data, old->len);
        if (!f) break;
        fpy_sb_add(&sb, p, f - p);
        fpy_sb_add_str(&sb, new_);
        p = f + old->len;
        done++;
    }
    fpy_sb_add(&sb, p, end - p);
    return fpy_sb_finish(&sb);
}

bool fpy_str_startswith(FpyStr *s, FpyStr *p) { return p->len <= s->len && memcmp(s->data, p->data, (size_t)p->len) == 0; }
bool fpy_str_endswith(FpyStr *s, FpyStr *p) {
    return p->len <= s->len && memcmp(s->data + s->len - p->len, p->data, (size_t)p->len) == 0;
}

static fpy_int byte_to_cp(FpyStr *s, int64_t off) {
    if (s->len == s->nchars) return off;
    return count_chars(s->data, off);
}

fpy_int fpy_str_find(FpyStr *s, FpyStr *sub) {
    const char *f = find_bytes(s->data, s->len, sub->data, sub->len);
    return f ? byte_to_cp(s, f - s->data) : -1;
}
fpy_int fpy_str_rfind(FpyStr *s, FpyStr *sub) {
    for (int64_t i = s->len - sub->len; i >= 0; i--)
        if (memcmp(s->data + i, sub->data, (size_t)sub->len) == 0) return byte_to_cp(s, i);
    return -1;
}
fpy_int fpy_str_index(FpyStr *s, FpyStr *sub) {
    fpy_int i = fpy_str_find(s, sub);
    if (i < 0) fpy_raise_code(FPY_EXC_VALUE, "substring not found");
    return i;
}
fpy_int fpy_str_count(FpyStr *s, FpyStr *sub) {
    if (sub->len == 0) return s->nchars + 1;
    fpy_int n = 0;
    const char *p = s->data, *end = s->data + s->len;
    for (;;) {
        const char *f = find_bytes(p, end - p, sub->data, sub->len);
        if (!f) break;
        n++;
        p = f + sub->len;
    }
    return n;
}

#define CLASSIFY(name, pred)                                                \
    bool name(FpyStr *s) {                                                  \
        if (s->len == 0) return false;                                      \
        for (int64_t i = 0; i < s->len; i++)                                \
            if (!(pred((unsigned char)s->data[i]))) return false;           \
        return true;                                                        \
    }
CLASSIFY(fpy_str_isdigit, isdigit)
CLASSIFY(fpy_str_isalpha, isalpha)
CLASSIFY(fpy_str_isalnum, isalnum)
CLASSIFY(fpy_str_isspace, isspace)

bool fpy_str_isupper(FpyStr *s) {
    bool cased = false;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (islower(c)) return false;
        if (isupper(c)) cased = true;
    }
    return cased;
}
bool fpy_str_islower(FpyStr *s) {
    bool cased = false;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (isupper(c)) return false;
        if (islower(c)) cased = true;
    }
    return cased;
}
bool fpy_str_isidentifier(FpyStr *s) {
    if (s->len == 0 || isdigit((unsigned char)s->data[0])) return false;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (!(isalnum(c) || c == '_' || c >= 0x80)) return false;
    }
    return true;
}

static FpyStr *justify(FpyStr *s, fpy_int w, FpyStr *fill, char align) {
    if (w <= s->nchars) return s;
    if (fill && fill->nchars != 1) fpy_raise_code(FPY_EXC_TYPE, "The fill character must be exactly one character long");
    FpySB sb;
    fpy_sb_init(&sb);
    int64_t padn = w - s->nchars, left = 0;
    if (align == '>') left = padn;
    else if (align == '^') left = padn / 2 + ((padn & 1) & (w & 1));
    const char *f = fill ? fill->data : " ";
    int64_t fl = fill ? fill->len : 1;
    for (int64_t i = 0; i < left; i++) fpy_sb_add(&sb, f, fl);
    fpy_sb_add_str(&sb, s);
    for (int64_t i = 0; i < padn - left; i++) fpy_sb_add(&sb, f, fl);
    return fpy_sb_finish(&sb);
}
FpyStr *fpy_str_center(FpyStr *s, fpy_int w, FpyStr *fill) { return justify(s, w, fill, '^'); }
FpyStr *fpy_str_ljust(FpyStr *s, fpy_int w, FpyStr *fill) { return justify(s, w, fill, '<'); }
FpyStr *fpy_str_rjust(FpyStr *s, fpy_int w, FpyStr *fill) { return justify(s, w, fill, '>'); }
FpyStr *fpy_str_zfill(FpyStr *s, fpy_int w) {
    if (w <= s->nchars) return s;
    FpySB sb;
    fpy_sb_init(&sb);
    int64_t i = 0;
    if (s->len && (s->data[0] == '-' || s->data[0] == '+')) { fpy_sb_add(&sb, s->data, 1); i = 1; }
    for (int64_t k = 0; k < w - s->nchars; k++) fpy_sb_add(&sb, "0", 1);
    fpy_sb_add(&sb, s->data + i, s->len - i);
    return fpy_sb_finish(&sb);
}
FpyStr *fpy_str_removeprefix(FpyStr *s, FpyStr *p) {
    return fpy_str_startswith(s, p) ? fpy_str_new(s->data + p->len, s->len - p->len) : s;
}
FpyStr *fpy_str_removesuffix(FpyStr *s, FpyStr *p) {
    return (p->len && fpy_str_endswith(s, p)) ? fpy_str_new(s->data, s->len - p->len) : s;
}

static FpyTuple *str_tuple3(FpyStr *a, FpyStr *b, FpyStr *c) {
    static const uint8_t kinds[3] = {FK_OBJ, FK_OBJ, FK_OBJ};
    FpyTuple *t = fpy_tuple_new(3, kinds);
    t->items[0] = fpy_wp(a);
    t->items[1] = fpy_wp(b);
    t->items[2] = fpy_wp(c);
    return t;
}
FpyTuple *fpy_str_partition(FpyStr *s, FpyStr *sep) {
    const char *f = find_bytes(s->data, s->len, sep->data, sep->len);
    if (!f) return str_tuple3(s, fpy_str_new("", 0), fpy_str_new("", 0));
    return str_tuple3(fpy_str_new(s->data, f - s->data), sep,
                      fpy_str_new(f + sep->len, s->len - (f - s->data) - sep->len));
}
FpyTuple *fpy_str_rpartition(FpyStr *s, FpyStr *sep) {
    for (int64_t i = s->len - sep->len; i >= 0; i--)
        if (memcmp(s->data + i, sep->data, (size_t)sep->len) == 0)
            return str_tuple3(fpy_str_new(s->data, i), sep, fpy_str_new(s->data + i + sep->len, s->len - i - sep->len));
    return str_tuple3(fpy_str_new("", 0), fpy_str_new("", 0), s);
}

static bool str_iter_next(FpyIter *it, fpy_word *out) {
    FpyStr *s = (FpyStr *)it->o1;
    if (it->i >= s->len) return false;
    int64_t l = cp_len_at(s, it->i);
    *out = fpy_wp(l == 1 ? single_chars[(uint8_t)s->data[it->i] & 0x7F] : fpy_str_new(s->data + it->i, l));
    if (l == 1 && (uint8_t)s->data[it->i] >= 128) *out = fpy_wp(fpy_str_new(s->data + it->i, 1));
    it->i += l;
    return true;
}
FpyIter *fpy_str_iter(FpyStr *s) {
    FpyIter *it = fpy_iter_new(str_iter_next, FK_OBJ);
    it->o1 = (FpyObj *)s;
    return it;
}

/* ------------------------------------------------------------------ bytes */
FpyBytes *fpy_bytes_new(const uint8_t *data, int64_t len) {
    FpyBytes *b = fpy_alloc(&fpy_type_bytes, sizeof(FpyBytes) + (size_t)len + 1);
    b->len = len;
    if (len && data) memcpy(b->data, data, (size_t)len); /* data == NULL: caller fills it in */
    b->data[len] = 0;
    return b;
}
FpyBytes *fpy_str_encode(FpyStr *s) { return fpy_bytes_new((const uint8_t *)s->data, s->len); }
FpyStr *fpy_bytes_decode(FpyBytes *b) { return fpy_str_new((const char *)b->data, b->len); }
bool fpy_bytes_eq(FpyBytes *a, FpyBytes *b) { return a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0; }
fpy_int fpy_bytes_getitem(FpyBytes *b, fpy_int i) { return b->data[fpy_norm_index(i, b->len, "bytes")]; }
FpyBytes *fpy_bytes_concat(FpyBytes *a, FpyBytes *b) {
    FpyBytes *r = fpy_bytes_new(a->data, a->len + b->len);
    memcpy(r->data + a->len, b->data, (size_t)b->len);
    return r;
}
FpyBytes *fpy_bytes_slice(FpyBytes *b, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step) {
    int64_t start, stop;
    slice_indices(b->len, has_lo, lo, has_hi, hi, step, &start, &stop);
    int64_t n = fpy_slice_len(start, stop, step);
    FpyBytes *r = fpy_bytes_new(NULL, n);
    for (int64_t k = 0, i = start; k < n; k++, i += step) r->data[k] = b->data[i];
    return r;
}
FpyBytes *fpy_bytes_repeat(FpyBytes *b, fpy_int n) {
    if (n <= 0 || b->len == 0) return fpy_bytes_new(NULL, 0);
    if (b->len > INT64_MAX / n) fpy_raise_code(FPY_EXC_MEMORY, "repeated bytes are too long");
    FpyBytes *r = fpy_bytes_new(NULL, b->len * n);
    for (fpy_int i = 0; i < n; i++) memcpy(r->data + i * b->len, b->data, (size_t)b->len);
    return r;
}
bool fpy_bytes_contains_byte(FpyBytes *b, fpy_int v) {
    if (v < 0 || v > 255) fpy_raise_code(FPY_EXC_VALUE, "byte must be in range(0, 256)");
    return b->len && memchr(b->data, (int)v, (size_t)b->len) != NULL;
}
bool fpy_bytes_contains(FpyBytes *b, FpyBytes *sub) {
    if (sub->len == 0) return true;
    for (int64_t i = 0; i + sub->len <= b->len; i++)
        if (memcmp(b->data + i, sub->data, (size_t)sub->len) == 0) return true;
    return false;
}
FpyIter *fpy_bytes_iter(FpyBytes *b) {
    FpyList *l = fpy_list_new(FK_INT, b->len);
    for (int64_t i = 0; i < b->len; i++) fpy_list_append(l, b->data[i]);
    return fpy_list_iter(l);
}
FpyStr *fpy_bytes_repr(FpyBytes *b) {
    FpySB sb;
    fpy_sb_init(&sb);
    fpy_sb_add(&sb, "b'", 2);
    for (int64_t i = 0; i < b->len; i++) {
        uint8_t c = b->data[i];
        char tmp[8];
        if (c == '\'' || c == '\\') { tmp[0] = '\\'; tmp[1] = (char)c; fpy_sb_add(&sb, tmp, 2); }
        else if (c == '\n') fpy_sb_add(&sb, "\\n", 2);
        else if (c == '\r') fpy_sb_add(&sb, "\\r", 2);
        else if (c == '\t') fpy_sb_add(&sb, "\\t", 2);
        else if (c < 32 || c >= 127) { int n = snprintf(tmp, sizeof tmp, "\\x%02x", c); fpy_sb_add(&sb, tmp, n); }
        else fpy_sb_add(&sb, (char *)&c, 1);
    }
    fpy_sb_add(&sb, "'", 1);
    return fpy_sb_finish(&sb);
}

bool fpy_str_istitle(FpyStr *s) {
    bool cased = false, prev_cased = false;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (isupper(c)) { if (prev_cased) return false; prev_cased = cased = true; }
        else if (islower(c)) { if (!prev_cased) return false; prev_cased = cased = true; }
        else prev_cased = false;
    }
    return cased;
}
bool fpy_str_isascii(FpyStr *s) { return s->len == s->nchars && !memchr(s->data, 0x80, 0) && ({ bool ok = true; for (int64_t i = 0; i < s->len; i++) if ((uint8_t)s->data[i] >= 128) { ok = false; break; } ok; }); }
bool fpy_str_isprintable(FpyStr *s) {
    for (int64_t i = 0; i < s->len; i++) { uint8_t c = (uint8_t)s->data[i]; if (c < 32 || c == 127) return false; }
    return true;
}

FpyStr *fpy_str_expandtabs(FpyStr *s, fpy_int tabsize) {
    FpySB sb;
    fpy_sb_init(&sb);
    int64_t col = 0;
    for (int64_t i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c == '\t') {
            if (tabsize > 0) {
                int64_t n = tabsize - col % tabsize;
                for (int64_t k = 0; k < n; k++) fpy_sb_add(&sb, " ", 1);
                col += n;
            }
        } else {
            fpy_sb_add(&sb, &c, 1);
            if (c == '\n' || c == '\r') col = 0;
            else if (((uint8_t)c & 0xC0) != 0x80) col++;
        }
    }
    return fpy_sb_finish(&sb);
}
