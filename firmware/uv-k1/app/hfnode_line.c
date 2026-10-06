/* hfnode serial lines: framing and checksums, with no dependence on the radio.
 *
 * Copyright 2026 the ic-7300-hf-server contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include <string.h>

#include "app/hfnode_line.h"

static const char HEX[] = "0123456789ABCDEF";

static bool printable(uint8_t c)
{
    return c >= 0x20 && c <= 0x7E;
}

bool HF_LineFeed(HF_LineReader_t *r, uint8_t byte)
{
    if (byte == '\n') {
        // A '\r' just before the '\n' is not part of the line.
        if (r->len > 0 && r->buf[r->len - 1] == '\r')
            r->len--;
        const bool ok = !r->bad && r->len > 0;
        r->buf[r->len] = 0;
        r->len = 0;
        r->bad = false;
        return ok;
    }
    // '\r' is only allowed as the byte before '\n'; keep it for now and let a
    // following byte other than '\n' mark the line bad.
    if (r->len > 0 && r->buf[r->len - 1] == '\r')
        r->bad = true;
    if (!(printable(byte) || byte == '\r') || r->len >= HF_LINE_MAX + (byte == '\r')) {
        r->bad = true;
        return false;
    }
    r->buf[r->len++] = (char)byte;
    return false;
}

static int hex_digit(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;   // lower case is not allowed
}

static int hex_byte(const char *s)
{
    const int hi = hex_digit(s[0]);
    const int lo = hex_digit(s[1]);
    return (hi < 0 || lo < 0) ? -1 : (hi << 4) | lo;
}

bool HF_LineParse(char *line, uint8_t *id, char **body)
{
    const size_t len = strlen(line);
    // "01 X*00" is the shortest line.
    if (len < 7 || line[2] != ' ' || line[len - 3] != '*')
        return false;
    const int i = hex_byte(line);
    const int cs = hex_byte(line + len - 2);
    if (i <= 0 || cs < 0)
        return false;
    uint8_t x = 0;
    for (size_t k = 0; k < len - 3; k++)
        x ^= (uint8_t)line[k];
    if (x != (uint8_t)cs)
        return false;
    line[len - 3] = 0;
    *id = (uint8_t)i;
    *body = line + 3;
    return **body != 0;
}

size_t HF_LineFormat(char *out, size_t cap, uint8_t id, const char *body)
{
    const size_t blen = strlen(body);
    // "<id> " + body + "*<cs>\n" + NUL
    const size_t len = 3 + blen + 4;
    if (len + 1 > cap || len - 1 > HF_LINE_MAX)
        return 0;
    out[0] = HEX[id >> 4];
    out[1] = HEX[id & 0xF];
    out[2] = ' ';
    uint8_t x = (uint8_t)out[0] ^ (uint8_t)out[1] ^ (uint8_t)' ';
    for (size_t k = 0; k < blen; k++) {
        if (!printable((uint8_t)body[k]))
            return 0;
        out[3 + k] = body[k];
        x ^= (uint8_t)body[k];
    }
    char *p = out + 3 + blen;
    *p++ = '*';
    *p++ = HEX[x >> 4];
    *p++ = HEX[x & 0xF];
    *p++ = '\n';
    *p = 0;
    return len;
}

bool HF_CwCharOk(char c)
{
    if ((c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9'))
        return true;
    return c != 0 && strchr(" .,?'/():=+-\"@", c) != NULL;
}
