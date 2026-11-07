/* Offline codec worker linked against the installed FreeRDP headers and library. */
#include <freerdp/codec/progressive.h>
#include <freerdp/codec/h264.h>
#include <freerdp/codec/region.h>
#include <winpr/wlog.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define LIMIT (256u * 1024u * 1024u)
typedef struct {
    BYTE* pixels;
    UINT32 width, height, prog_width, prog_height;
    BOOL progressive;
    H264_CONTEXT* h264;
} Surface;
static Surface surfaces[65536];
static size_t allocated;
static PROGRESSIVE_CONTEXT* progressive;
static void fail(const char* message) { fprintf(stderr, "%s\n", message); exit(2); }
static UINT32 read32(void) {
    BYTE b[4]; if (fread(b, 1, 4, stdin) != 4) fail("short codec request");
    return (UINT32)b[0] | ((UINT32)b[1] << 8) | ((UINT32)b[2] << 16) | ((UINT32)b[3] << 24);
}
static UINT16 read16(void) {
    BYTE b[2]; if (fread(b, 1, 2, stdin) != 2) fail("short rectangle");
    return (UINT16)(b[0] | ((UINT16)b[1] << 8));
}
static void write32(UINT32 value) {
    BYTE b[4] = {(BYTE)value, (BYTE)(value >> 8), (BYTE)(value >> 16), (BYTE)(value >> 24)};
    if (fwrite(b, 1, 4, stdout) != 4) fail("failed to write codec response");
}
static void write16(UINT16 value) {
    BYTE b[2] = {(BYTE)value, (BYTE)(value >> 8)};
    if (fwrite(b, 1, 2, stdout) != 2) fail("failed to write rectangle");
}
static BYTE* data(UINT32* size) {
    *size = read32(); if (*size > 64u * 1024u * 1024u) fail("codec input exceeds limit");
    BYTE* p = malloc(*size ? *size : 1);
    if (!p || fread(p, 1, *size, stdin) != *size) fail("short codec input");
    return p;
}
static void clear_pixels(Surface* s) {
    if (s->pixels) allocated -= (size_t)s->width * s->height * 4;
    free(s->pixels); s->pixels = NULL;
    if (s->h264) h264_context_free(s->h264);
    s->h264 = NULL; s->width = s->height = 0;
}
static void create(UINT16 id, UINT32 width, UINT32 height) {
    Surface* s = &surfaces[id];
    if (!width || !height || width > 16384 || height > 16384) fail("invalid codec surface dimensions");
    clear_pixels(s);
    size_t bytes = (size_t)width * height * 4;
    if (bytes > LIMIT || allocated > LIMIT - bytes) fail("codec surfaces exceed memory limit");
    s->pixels = calloc(1, bytes); if (!s->pixels) fail("cannot allocate codec surface");
    allocated += bytes; s->width = width; s->height = height;
    if (!s->progressive || s->prog_width != width || s->prog_height != height) {
        if (s->progressive) progressive_delete_surface_context(progressive, id);
        if (progressive_create_surface_context(progressive, id, width, height) < 0) fail("cannot create Progressive context");
        s->progressive = TRUE; s->prog_width = width; s->prog_height = height;
    }
}
static void response(INT32 status, Surface* s, const RECTANGLE_16* rectangles, UINT32 count) {
    write32((UINT32)status); write32(status < 0 ? 0 : count);
    if (status >= 0) for (UINT32 i = 0; i < count; i++) {
        const RECTANGLE_16* r = &rectangles[i];
        if (!s || !s->pixels || r->left >= r->right || r->top >= r->bottom || r->right > s->width || r->bottom > s->height)
            fail("FreeRDP returned an invalid rectangle");
        write16(r->left); write16(r->top); write16(r->right); write16(r->bottom);
        for (UINT32 y = r->top; y < r->bottom; y++) {
            size_t pixels = r->right - r->left;
            if (fwrite(s->pixels + ((size_t)y * s->width + r->left) * 4, 4, pixels, stdout) != pixels)
                fail("failed to write decoded pixels");
        }
    }
    if (fflush(stdout) != 0) fail("cannot flush codec response");
}
int main(void) {
    if (!WLog_SetLogLevel(WLog_GetRoot(), WLOG_OFF)) fail("cannot disable codec stdout logging");
    progressive = progressive_context_new_ex(FALSE, 0);
    if (!progressive) fail("cannot create Progressive decoder");
    for (;;) {
        int first = fgetc(stdin); if (first == EOF) break;
        if (ungetc(first, stdin) == EOF) fail("cannot read codec request");
        UINT32 op = read32(), raw_id = read32();
        if (raw_id > 65535) fail("invalid surface id");
        UINT16 id = (UINT16)raw_id; Surface* s = &surfaces[id];
        if (op == 0) {
            UINT32 width = read32(), height = read32(); create(id, width, height);
            response(1, NULL, NULL, 0);
        } else if (op == 1) {
            clear_pixels(s);
            if (s->progressive) progressive_delete_surface_context(progressive, id);
            memset(s, 0, sizeof(*s)); response(1, NULL, NULL, 0);
        } else if (op == 2) {
            /* ResetGraphics ends H.264 streams; Progressive state lasts until surface deletion. */
            for (size_t i = 0; i < 65536; i++) clear_pixels(&surfaces[i]);
            response(1, NULL, NULL, 0);
        } else if (op == 3) {
            UINT32 frame = read32(), size; BYTE* input = data(&size);
            if (!s->pixels) fail("Progressive update without surface");
            REGION16 invalid; region16_init(&invalid);
            INT32 status = progressive_decompress(progressive, input, size, s->pixels,
                PIXEL_FORMAT_RGBA32, s->width * 4, 0, 0, &invalid, id, frame);
            UINT32 count = 0; const RECTANGLE_16* rects = region16_rects(&invalid, &count);
            response(status, s, rects, count); region16_uninit(&invalid); free(input);
        } else if (op == 4) {
            UINT32 count = read32();
            if (!count || count > 65535 || !s->pixels) fail("invalid AVC420 region count or surface");
            RECTANGLE_16* rects = calloc(count, sizeof(*rects)); if (!rects) fail("cannot allocate AVC420 rectangles");
            for (UINT32 i = 0; i < count; i++) {
                rects[i].left = read16(); rects[i].top = read16(); rects[i].right = read16(); rects[i].bottom = read16();
            }
            UINT32 size; BYTE* input = data(&size);
            if (!s->h264) {
                s->h264 = h264_context_new(FALSE);
                if (!s->h264 || !h264_context_reset(s->h264, s->width, s->height)) fail("cannot create H.264 decoder");
            }
            INT32 status = avc420_decompress(s->h264, input, size, s->pixels, PIXEL_FORMAT_RGBA32,
                s->width * 4, s->width, s->height, rects, count);
            response(status, s, rects, count); free(input); free(rects);
        } else fail("unsupported codec worker request");
    }
    for (size_t i = 0; i < 65536; i++) clear_pixels(&surfaces[i]);
    progressive_context_free(progressive);
    return 0;
}
