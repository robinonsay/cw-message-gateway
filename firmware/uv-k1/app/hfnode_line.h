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

// The line format of hfnode's docs/handheld-protocol.md:
//
//     <id> <body>*<cs>\n
//
// <id> two upper-case hex digits 01-FF, <cs> two upper-case hex digits, the XOR of
// every byte before the '*'. Printable ASCII only, at most 80 characters before the
// '\n', and a '\r' before the '\n' is ignored. Anything else is not a line.

#ifndef APP_HFNODE_LINE_H
#define APP_HFNODE_LINE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define HF_LINE_MAX 80

// Collects bytes into lines.
typedef struct {
    char    buf[HF_LINE_MAX + 2];   // and a '\r', and the NUL
    uint8_t len;
    bool    bad;    // over-long or a byte outside printable ASCII
} HF_LineReader_t;

// Feed one byte. Returns true when `r->buf` holds a complete line (without its
// '\r\n', NUL-terminated) that is printable and short enough; the line is consumed
// by the next call.
bool HF_LineFeed(HF_LineReader_t *r, uint8_t byte);

// Check a complete line's id and checksum. On success sets `*id` and `*body` (into
// `line`, NUL-terminated at the '*', which is overwritten) and returns true.
bool HF_LineParse(char *line, uint8_t *id, char **body);

// Write "<id> <body>*<cs>\n" to `out` (NUL-terminated); returns its length without
// the NUL, or 0 if it does not fit in `cap` or `body` is not printable.
size_t HF_LineFormat(char *out, size_t cap, uint8_t id, const char *body);

// Whether `c` may be keyed: A-Z 0-9 space . , ? ' / ( ) : = + - " @
bool HF_CwCharOk(char c);

#endif
