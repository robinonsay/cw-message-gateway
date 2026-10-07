/* Host test of hfnode's firmware side (app/hfnode.c, app/hfnode_line.c) against a
 * simulated radio: its line format, its commands, and above all the limits that
 * stop a transmission. Builds and runs on any computer with a C compiler; nothing
 * here touches a radio or the firmware's own code. See ../README.md.
 *
 *   cc -std=c11 -Wall -Wextra -Werror -I test/stubs -I . \
 *      test/test_hfnode.c app/hfnode_line.c -o test_hfnode && ./test_hfnode
 *
 * (from firmware/uv-k1)
 *
 * Copyright 2026 the ic-7300-hf-server contributors
 * Licensed under the Apache License, Version 2.0 (see app/hfnode.c).
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "fake_radio.h"

// TEST HANG stops the main loop for good on the radio; here it is counted.
static int g_hangs;
static void test_hang(void) { g_hangs++; }
#define HF_HANG() test_hang()

// The code under test, with its static state in reach.
#include "app/hfnode.c"

// STATUS's charging field when not charging: unknown on a board that cannot tell.
#if HF_CHARGE_SENSED
#define NOT_CHARGING " 0"
#else
#define NOT_CHARGING " ?"
#endif

// ---------------------------------------------------------------------------
// The simulated radio
// ---------------------------------------------------------------------------

static uint32_t g_now;      // ms
static uint32_t g_auto_ms;  // added to the clock on every read, for busy-waits

FUNCTION_Type_t gCurrentFunction;
uint8_t         gCW_State;
bool            g_SquelchLost;
bool            gCW_PlaybackActive;
bool            gCW_Recording;
uint8_t         gCW_MessageRepeatCountdown_500ms;
bool            gChargingWithTypeC;
EEPROM_Config_t gEeprom;
const char      gModulationStr[MODULATION_UKNOWN][4] = {"FM", "AM", "USB", "CW"};

static FREQ_Config_t g_rx_freq, g_tx_freq;
static VFO_Info_t    g_vfo_tx, g_vfo_rx;
VFO_Info_t          *gTxVfo = &g_vfo_tx;
VFO_Info_t          *gRxVfo = &g_vfo_rx;

uint8_t           VCP_RxBuf[VCP_RX_BUF_SIZE];
volatile uint32_t VCP_RxBufPointer;

static bool g_reg30_tx;          // the BK4819's transmit DSP
static bool g_end_tx_fails;      // CW_EndTxNow leaves the transmitter on
static char g_played[64];        // the last text given to the keyer
static int  g_played_wpm;
static int  g_end_tx_calls, g_stop_playback_calls;
static bool g_usb_busy;
static char g_usb_out[4096];     // everything written to the host
static int  g_wdg_reloads;
static bool g_wdg_enabled;

uint32_t millis(void)
{
    g_now += g_auto_ms;
    return g_now;
}

uint32_t millis_since(uint32_t prev) { return millis() - prev; }

uint16_t BK4819_ReadRegister(uint8_t reg)
{
    return reg == BK4819_REG_30 && g_reg30_tx ? BK4819_REG_30_ENABLE_TX_DSP : 0;
}

bool CW_StartTextPlayback(const char *text, uint8_t wpm)
{
    if (gCW_Recording || gCW_PlaybackActive)
        return false;
    snprintf(g_played, sizeof g_played, "%s", text);
    g_played_wpm = wpm;
    gCW_PlaybackActive = true;
    return true;
}

void CW_StopPlayback(void)
{
    g_stop_playback_calls++;
    gCW_PlaybackActive = false;
    gCW_MessageRepeatCountdown_500ms = 0;
}

void RADIO_CW_Suspend(void) { gCW_State = CW_SUSPENDED; }

void CW_EndTxNow(void)
{
    g_end_tx_calls++;
    if (g_end_tx_fails)
        return;
    gCW_State = CW_INACTIVE;
    gCurrentFunction = FUNCTION_FOREGROUND;
    g_reg30_tx = false;
}

int usbd_ep_start_write(uint8_t ep, const uint8_t *data, uint32_t len)
{
    if (ep != HF_CDC_IN_EP)
        abort();
    if (g_usb_busy)
        return -3;
    strncat(g_usb_out, (const char *)data, len);
    return 0;
}

void LL_IWDG_Enable(void *iwdg) { (void)iwdg; g_wdg_enabled = true; }
void LL_IWDG_EnableWriteAccess(void *iwdg) { (void)iwdg; }
void LL_IWDG_SetPrescaler(void *iwdg, uint32_t p) { (void)iwdg; (void)p; }
void LL_IWDG_SetReloadCounter(void *iwdg, uint32_t c) { (void)iwdg; (void)c; }
uint32_t LL_IWDG_IsReady(void *iwdg) { (void)iwdg; return 1; }
void LL_IWDG_ReloadCounter(void *iwdg) { (void)iwdg; g_wdg_reloads++; }
void LL_RCC_LSI_Enable(void) {}

// The keyer keys the carrier, as the CW engine does on its first element.
static void key_down(void)
{
    gCW_State = CW_TRANSMITTING;
    gCurrentFunction = FUNCTION_TRANSMIT;
    g_reg30_tx = true;
}

// The text has gone out and the break-in tail is over.
static void text_done(void)
{
    gCW_PlaybackActive = false;
    gCW_State = CW_INACTIVE;
    gCurrentFunction = FUNCTION_FOREGROUND;
    g_reg30_tx = false;
}

// A radio as the operator leaves it for hfnode: CW on 144.060 MHz, break-in on.
static void reset_radio(void)
{
    memset(&s_reader, 0, sizeof s_reader);
    s_rx_read = 0;
    s_txq_head = s_txq_count = 0;
    s_run = RUN_NONE;
    s_run_armed = s_wdg_starve = s_wdg_started = false;
    s_duty_ms = 0;
    s_readback_bad = false;

    g_now = 1000;
    s_ms = 1000;   // SysTick's count, kept with the clock by run_for
    g_auto_ms = 0;
    gCurrentFunction = FUNCTION_FOREGROUND;
    gCW_State = CW_INACTIVE;
    g_SquelchLost = false;
    gCW_PlaybackActive = gCW_Recording = false;
    gCW_MessageRepeatCountdown_500ms = 0;
    gChargingWithTypeC = false;
    gEeprom.CW_BREAKIN_ENABLE = true;
    g_rx_freq.Frequency = g_tx_freq.Frequency = 14406000;
    g_vfo_tx = (VFO_Info_t){&g_rx_freq, &g_tx_freq, MODULATION_CW, OUTPUT_POWER_LOW1};
    g_vfo_rx = g_vfo_tx;
    memset(VCP_RxBuf, 0, sizeof VCP_RxBuf);
    VCP_RxBufPointer = 0;
    g_reg30_tx = g_end_tx_fails = g_usb_busy = false;
    g_played[0] = 0;
    g_end_tx_calls = g_stop_playback_calls = g_wdg_reloads = g_hangs = 0;
    g_usb_out[0] = 0;
    HFNODE_Init();
}

// What the USB interrupt does with bytes from the host.
static void host_bytes(const char *s, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        VCP_RxBuf[VCP_RxBufPointer] = (uint8_t)s[i];
        VCP_RxBufPointer = (VCP_RxBufPointer + 1) % VCP_RX_BUF_SIZE;
    }
}

static void host_send(uint8_t id, const char *body)
{
    char line[96];
    const size_t n = HF_LineFormat(line, sizeof line, id, body);
    if (n == 0)
        abort();
    host_bytes(line, n);
}

// The main loop, once.
static void loop(void) { HFNODE_Poll(); }

// `ms` of time, with the main loop running every millisecond (unless `hung`) and
// SysTick every 10.
static void run_for(uint32_t ms, bool hung)
{
    for (uint32_t i = 0; i < ms; i++) {
        g_now++;
        if (g_now % 10 == 0)
            HFNODE_WatchdogTick();
        if (!hung)
            loop();
    }
}

// The next reply line from the firmware, checked and taken off the output; ""
// if there is none.
static const char *next_reply(uint8_t *id)
{
    static char line[128];
    char *nl = strchr(g_usb_out, '\n');
    if (!nl)
        return "";
    const size_t n = (size_t)(nl - g_usb_out);
    memcpy(line, g_usb_out, n);
    line[n] = 0;
    memmove(g_usb_out, nl + 1, strlen(nl + 1) + 1);
    char *body;
    if (!HF_LineParse(line, id, &body)) {
        fprintf(stderr, "bad line from the firmware: %s\n", line);
        abort();
    }
    return body;
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

static int g_failed;

#define CHECK(cond)                                                                      \
    do {                                                                                 \
        if (!(cond)) {                                                                   \
            fprintf(stderr, "%s:%d: %s: failed: %s\n", __FILE__, __LINE__, __func__,     \
                    #cond);                                                              \
            g_failed++;                                                                  \
        }                                                                                \
    } while (0)

// Send `body` as `id`, run the main loop once, and check the reply.
static void expect(uint8_t id, const char *body, const char *reply)
{
    host_send(id, body);
    loop();
    uint8_t got;
    const char *r = next_reply(&got);
    if (strcmp(r, reply) || (reply[0] && got != id)) {
        fprintf(stderr, "  %02X %s -> %02X %s, expected %s\n", id, body, got, r, reply);
        g_failed++;
    }
}

static void expect_reply(uint8_t id, const char *reply)
{
    uint8_t got = 0;
    const char *r = next_reply(&got);
    if (strcmp(r, reply) || got != id) {
        fprintf(stderr, "  reply %02X %s, expected %02X %s\n", got, r, id, reply);
        g_failed++;
    }
}

static bool status_tx(void)
{
    host_send(0x7F, "STATUS");
    loop();
    uint8_t id;
    const char *r = next_reply(&id);
    if (!strncmp(r, "OK STATUS 1 ", 12))
        return true;
    if (strncmp(r, "OK STATUS 0 ", 12)) {
        fprintf(stderr, "  STATUS -> %s\n", r);
        g_failed++;
    }
    return false;
}

// A CW run accepted and keyed: replied OK CW.
static void start_run(uint8_t id, const char *text)
{
    char cmd[64];
    snprintf(cmd, sizeof cmd, "CW 20 %s", text);
    host_send(id, cmd);
    loop();
    CHECK(next_reply(&(uint8_t){0})[0] == 0);   // no reply until keyed
    CHECK(s_run == RUN_STARTING);
    run_for(20, false);
    key_down();
    loop();
    expect_reply(id, "OK CW");
    CHECK(s_run == RUN_KEYING);
}

// ---------------------------------------------------------------------------
// The line format
// ---------------------------------------------------------------------------

static bool feed(HF_LineReader_t *r, const char *s)
{
    bool done = false;
    for (; *s; s++)
        done = HF_LineFeed(r, (uint8_t)*s);
    return done;
}

static void test_line_format(void)
{
    char out[96];
    CHECK(HF_LineFormat(out, sizeof out, 1, "STATUS") == 13);
    CHECK(!strcmp(out, "01 STATUS*35\n"));   // as in docs/handheld-protocol.md
    CHECK(HF_LineFormat(out, sizeof out, 1, "HELLO") == 12);
    CHECK(!strcmp(out, "01 HELLO*63\n"));
    CHECK(HF_LineFormat(out, 12, 1, "HELLO") == 0);   // no room for the NUL
    CHECK(HF_LineFormat(out, sizeof out, 1, "OK\x01") == 0);
    char body[90];
    memset(body, 'A', sizeof body);
    body[73] = 0;   // 3 + 73 + 3 = 79 characters: fits in 80
    CHECK(HF_LineFormat(out, sizeof out, 0xFF, body) == 80);
    body[73] = 'A';
    body[74] = 0;   // 80 characters
    CHECK(HF_LineFormat(out, sizeof out, 0xFF, body) == 81);
    body[74] = 'A';
    body[75] = 0;   // 81: too long
    CHECK(HF_LineFormat(out, sizeof out, 0xFF, body) == 0);
}

static void test_line_reader(void)
{
    HF_LineReader_t r = {0};
    CHECK(feed(&r, "01 STATUS*35\n") && !strcmp(r.buf, "01 STATUS*35"));
    CHECK(feed(&r, "01 STATUS*35\r\n") && !strcmp(r.buf, "01 STATUS*35"));
    CHECK(!feed(&r, "\n"));                         // empty
    CHECK(!feed(&r, "01 ST\rATUS*35\n"));           // \r inside
    CHECK(feed(&r, "01 STATUS*35\n"));              // and the next line is fine
    CHECK(!feed(&r, "01 ST\x80TUS*35\n"));          // not printable
    CHECK(!feed(&r, "01 ST\tTUS*35\n"));
    char longline[100];
    memset(longline, 'A', 81);
    longline[81] = '\n';
    longline[82] = 0;
    CHECK(!feed(&r, longline));                     // 81 characters
    longline[80] = '\n';
    longline[81] = 0;
    CHECK(feed(&r, longline) && strlen(r.buf) == 80);
    longline[80] = '\r';
    longline[81] = '\n';
    longline[82] = 0;
    CHECK(feed(&r, longline) && strlen(r.buf) == 80);   // 80 and a \r
}

static void test_line_parse(void)
{
    char line[96];
    uint8_t id;
    char *body;
    strcpy(line, "01 STATUS*35");
    CHECK(HF_LineParse(line, &id, &body) && id == 1 && !strcmp(body, "STATUS"));
    strcpy(line, "2A CW 20 CQ DE N0CALL*5E");
    CHECK(!HF_LineParse(line, &id, &body));         // the checksum is not 5E
    HF_LineFormat(line, sizeof line, 0x2A, "CW 20 CQ DE N0CALL");
    line[strlen(line) - 1] = 0;
    CHECK(HF_LineParse(line, &id, &body) && id == 0x2A && !strcmp(body, "CW 20 CQ DE N0CALL"));
    strcpy(line, "01 STATUS*36");
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "01 STOQ*35");
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "00 STATUS*34");                   // id 00 is not an id
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "0a STATUS*55");                   // lower-case hex
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "01STATUS*15");
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "01 *11");                         // empty body
    CHECK(!HF_LineParse(line, &id, &body));
    strcpy(line, "01 STATUS");
    CHECK(!HF_LineParse(line, &id, &body));
}

static void test_cw_chars(void)
{
    const char *ok = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,?'/():=+-\"@";
    for (int c = 1; c < 256; c++)
        CHECK(HF_CwCharOk((char)c) == (strchr(ok, c) != NULL));
    CHECK(!HF_CwCharOk(0));
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

static void test_queries(void)
{
    reset_radio();
    expect(1, "HELLO", "OK HELLO 2 60 2000 0 NR7Y-CW HFNODE");
    expect(2, "STATUS", "OK STATUS 0 0" NOT_CHARGING);
    expect(3, "FREQ", "OK FREQ 144060000 144060000");
    g_tx_freq.Frequency = 14466000;
    expect(4, "FREQ", "OK FREQ 144060000 144660000");
    expect(5, "MODE", "OK MODE CW CW");
    g_vfo_rx.Modulation = MODULATION_FM;
    g_vfo_tx.Modulation = MODULATION_USB;
    expect(6, "MODE", "OK MODE USB FM");
    g_vfo_tx.Modulation = MODULATION_UKNOWN;
    expect(7, "MODE", "OK MODE OTHER FM");
    expect(8, "POWER", "OK POWER LOW1");
    g_vfo_tx.OUTPUT_POWER = OUTPUT_POWER_HIGH;
    expect(9, "POWER", "OK POWER HIGH");
    g_vfo_tx.OUTPUT_POWER = 99;
    expect(10, "POWER", "OK POWER OTHER");
    expect(11, "BREAKIN", "OK BREAKIN 1");
    gEeprom.CW_BREAKIN_ENABLE = false;
    expect(12, "BREAKIN", "OK BREAKIN 0");
    // Nothing is set from a line: a query with an argument is not a command.
    expect(13, "FREQ 144070000", "ERR FREQ UNKNOWN");
    expect(14, "MODE CW", "ERR MODE UNKNOWN");
    expect(15, "POWER LOW", "ERR POWER UNKNOWN");
    expect(16, "HELLO THERE", "ERR HELLO UNKNOWN");
    expect(17, "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "ERR ABCDEFGHIJKLMNOPQRST UNKNOWN");
    expect(18, "TEST", "ERR TEST UNKNOWN");
    CHECK(g_tx_freq.Frequency == 14466000);
    // How long since it started, by its own clock.
    run_for(12345, false);
    expect(19, "HELLO", "OK HELLO 2 60 2000 12340 NR7Y-CW HFNODE");
}

static void test_damaged_lines_do_nothing(void)
{
    reset_radio();
    host_bytes("01 HELLO*64\n", 12);   // wrong checksum
    host_bytes("02 hello*03\n", 12);
    host_bytes("\x80\x81\n", 3);
    loop();
    CHECK(g_usb_out[0] == 0);
    // Bytes split across passes still make a line, across the end of the ring.
    VCP_RxBufPointer = s_rx_read = 250;
    host_bytes("03 HEL", 6);
    loop();
    host_bytes("LO*61\n", 6);
    CHECK(VCP_RxBufPointer == 6);
    loop();
    uint8_t id;
    CHECK(!strncmp(next_reply(&id), "OK HELLO", 8) && id == 3);
    // The USB interrupt can leave the pointer at the end for a moment: that is 0.
    VCP_RxBufPointer = s_rx_read = 250;
    host_bytes("04 HEL", 6);
    VCP_RxBufPointer = VCP_RX_BUF_SIZE;
    loop();
    CHECK(s_rx_read == 0);
    VCP_RxBufPointer = 0;
    host_bytes("LO*66\n", 6);
    loop();
    CHECK(!strncmp(next_reply(&id), "OK HELLO", 8) && id == 4);
}

static void test_cw_refusals(void)
{
    reset_radio();
    expect(1, "CW 20", "ERR CW LEN");
    expect(2, "CW 4 TEST", "ERR CW WPM");
    expect(3, "CW 51 TEST", "ERR CW WPM");
    expect(4, "CW 2A TEST", "ERR CW WPM");
    expect(5, "CW 020 TEST", "ERR CW WPM");
    expect(6, "CW 20 ", "ERR CW LEN");
    expect(7, "CW 20 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "ERR CW LEN");   // 31
    expect(8, "CW 20 test", "ERR CW CHAR");
    expect(9, "CW 20 A&B", "ERR CW CHAR");
    g_vfo_tx.Modulation = MODULATION_FM;
    expect(10, "CW 20 TEST", "ERR CW MODE");
    g_vfo_tx.Modulation = MODULATION_CW;
    gEeprom.CW_BREAKIN_ENABLE = false;
    expect(11, "CW 20 TEST", "ERR CW BKIN");
    gEeprom.CW_BREAKIN_ENABLE = true;
    gCW_Recording = true;
    expect(12, "CW 20 TEST", "ERR CW TX");
    gCW_Recording = false;
    gCurrentFunction = FUNCTION_TRANSMIT;   // the PTT held
    expect(13, "CW 20 TEST", "ERR CW TX");
    gCurrentFunction = FUNCTION_FOREGROUND;
    CHECK(g_played[0] == 0 && s_run == RUN_NONE);
    expect(14, "CW 20 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "");   // 30: accepted
    CHECK(!strcmp(g_played, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA") && g_played_wpm == 20);
    expect(15, "CW 20 TEST", "ERR CW TX");   // one run at a time
}

static void test_a_run_ends_with_its_text(void)
{
    reset_radio();
    start_run(0x21, "CQ DE N0CALL");
    CHECK(status_tx());
    // Kept alive, it runs on past the link timeout.
    for (int i = 0; i < 12; i++) {
        run_for(250, false);
        CHECK(status_tx());
    }
    CHECK(s_run == RUN_KEYING && g_end_tx_calls == 0);
    // The text is out but the break-in tail is not: still sending.
    gCW_PlaybackActive = false;
    gCW_State = CW_SUSPENDED;
    loop();
    CHECK(status_tx());
    text_done();
    loop();
    CHECK(!status_tx());
    CHECK(s_run == RUN_NONE && !s_run_armed && g_end_tx_calls == 0);
}

static void test_the_link_timeout_ends_a_run(void)
{
    reset_radio();
    start_run(1, "TEST");
    // Damaged lines do not keep the link alive.
    for (int i = 0; i < 7; i++) {
        run_for(250, false);
        host_bytes("02 STATUS*00\n", 13);
    }
    CHECK(s_run == RUN_KEYING);
    run_for(260, false);   // past 2 s from the CW line, the last good one
    CHECK(s_run == RUN_STOPPING && g_end_tx_calls == 1 && g_stop_playback_calls == 1);
    CHECK(!g_reg30_tx && !gCW_PlaybackActive);
    CHECK(!status_tx());
    // Watched for a second, then over; the watchdog fed all along.
    run_for(1000, false);
    CHECK(s_run == RUN_NONE && !s_run_armed && !s_wdg_starve);
}

static void test_the_transmit_limit_ends_a_run(void)
{
    reset_radio();
    const uint32_t accepted = g_now;
    start_run(1, "TEST");
    while (s_run == RUN_KEYING && g_now - accepted < 70000) {
        run_for(250, false);
        status_tx();
    }
    CHECK(s_run == RUN_STOPPING && g_end_tx_calls == 1);
    // 60 s from the CW being accepted, within the last 250 ms run here.
    const uint32_t lasted = g_now - accepted;
    CHECK(lasted >= 60000 && lasted <= 60250);
}

static void test_a_cw_that_never_keys_is_refused(void)
{
    reset_radio();
    expect(5, "CW 20 TEST", "");
    run_for(299, false);
    CHECK(g_usb_out[0] == 0 && s_run == RUN_STARTING);
    run_for(2, false);
    expect_reply(5, "ERR CW REFUSED");
    CHECK(s_run == RUN_STOPPING && !gCW_PlaybackActive);
    // Stopped, so watched for a second, and CW refused meanwhile.
    expect(6, "CW 20 TEST", "ERR CW WAIT");
    run_for(1000, false);
    CHECK(s_run == RUN_NONE);
    // A paddle stopping the playback before it keyed: refused at once.
    expect(7, "CW 20 TEST", "");
    gCW_PlaybackActive = false;
    loop();
    expect_reply(7, "ERR CW REFUSED");
}

static void test_stop(void)
{
    reset_radio();
    // Before the first key-down: the CW is answered too.
    expect(1, "CW 20 TEST", "");
    host_send(2, "STOP");
    loop();
    expect_reply(1, "ERR CW STOP");
    expect_reply(2, "OK STOP");
    CHECK(s_run == RUN_STOPPING && !gCW_PlaybackActive);
    run_for(1000, false);
    CHECK(s_run == RUN_NONE);
    // Keying: off at once, as STATUS says, and watched for a second.
    start_run(3, "TEST");
    expect(4, "STOP", "OK STOP");
    CHECK(s_run == RUN_STOPPING && !g_reg30_tx && gCW_State == CW_INACTIVE);
    CHECK(!status_tx());
    expect(8, "CW 20 TEST", "ERR CW WAIT");
    run_for(990, false);
    CHECK(s_run == RUN_STOPPING);
    run_for(10, false);
    CHECK(s_run == RUN_NONE && !s_wdg_starve);
    // With nothing running: the CW engine keyed by hand, say, is stopped and
    // watched too.
    key_down();
    expect(5, "STOP", "OK STOP");
    CHECK(gCW_State == CW_INACTIVE && !g_reg30_tx && s_run == RUN_STOPPING);
    run_for(1000, false);
    CHECK(s_run == RUN_NONE && !s_wdg_starve);
    expect(6, "STOP", "OK STOP");
    CHECK(s_run == RUN_NONE);   // nothing on: nothing to watch
    // A transmission in another mode is the operator's.
    g_vfo_tx.Modulation = MODULATION_FM;
    gCurrentFunction = FUNCTION_TRANSMIT;
    g_reg30_tx = true;
    const int ends = g_end_tx_calls;
    expect(7, "STOP", "OK STOP");
    CHECK(g_end_tx_calls == ends && status_tx());
}

static void test_a_stop_that_fails_resets_the_radio(void)
{
    reset_radio();
    run_for(100, false);
    CHECK(g_wdg_reloads > 0);
    start_run(1, "TEST");
    g_end_tx_fails = true;
    expect(2, "STOP", "OK STOP");
    CHECK(s_run == RUN_STOPPING && status_tx());
    // Still tried, and the watchdog still fed, for the grace time.
    run_for(400, false);
    CHECK(s_run == RUN_STOPPING && g_end_tx_calls > 100 && !s_wdg_starve);
    run_for(100, false);
    CHECK(s_wdg_starve);
    const int fed = g_wdg_reloads;
    // The main loop goes on, kept alive; the watchdog is not fed again.
    for (int i = 0; i < 20; i++) {
        run_for(250, false);
        status_tx();
    }
    CHECK(g_wdg_reloads == fed);
    // Off in the end (say, the BK4819 stopped by itself): the run ends, but the
    // reset still comes: a radio that failed to stop once is not trusted again.
    g_end_tx_fails = false;
    text_done();
    loop();
    CHECK(s_run == RUN_NONE && !status_tx());
    run_for(100, false);
    CHECK(g_wdg_reloads == fed);
}

// The node goes on sending STOP while the transmitter will not go off: each STOP
// must not put the reset off.
static void test_repeated_stops_do_not_put_off_the_reset(void)
{
    reset_radio();
    start_run(1, "TEST");
    g_end_tx_fails = true;
    for (uint8_t id = 2; id < 7; id++) {
        expect(id, "STOP", "OK STOP");
        run_for(100, false);
    }
    CHECK(s_run == RUN_STOPPING && status_tx());
    CHECK(s_wdg_starve);   // 500 ms from the first STOP
    const int fed = g_wdg_reloads;
    for (uint8_t id = 7; id < 40; id++) {
        expect(id, "STOP", "OK STOP");
        run_for(100, false);
    }
    CHECK(g_wdg_reloads == fed);
}

// A paddle held (or stuck) keys the radio again after the keyer is stopped.
static void test_a_held_paddle_after_a_stop_resets_the_radio(void)
{
    reset_radio();
    start_run(1, "TEST");
    expect(2, "STOP", "OK STOP");
    CHECK(!status_tx());
    // On again within the grace time: stopped again, no reset yet.
    run_for(300, false);
    key_down();
    loop();
    CHECK(!g_reg30_tx && gCW_State == CW_INACTIVE && !s_wdg_starve);
    // On again after it: the watchdog is no longer fed.
    run_for(300, false);
    key_down();
    loop();
    CHECK(s_wdg_starve && !g_reg30_tx);
    // After a run's watch, a STOP from the node that finds it keyed is watched
    // the same way.
    reset_radio();
    start_run(1, "TEST");
    text_done();
    loop();
    CHECK(s_run == RUN_NONE);
    key_down();
    expect(2, "STOP", "OK STOP");
    CHECK(s_run == RUN_STOPPING && s_run_armed);
    run_for(600, false);
    key_down();
    loop();
    CHECK(s_wdg_starve);
}

static void test_a_stop_by_a_limit_is_checked_too(void)
{
    reset_radio();
    start_run(1, "TEST");
    g_end_tx_fails = true;
    run_for(2100, false);   // the link timeout, with no keep-alive
    CHECK(s_run == RUN_STOPPING || s_wdg_starve);
    run_for(600, false);
    CHECK(s_wdg_starve);
}

static void test_switching_out_of_cw_ends_a_run(void)
{
    reset_radio();
    expect(1, "CW 20 TEST", "");
    g_vfo_tx.Modulation = MODULATION_FM;
    loop();
    expect_reply(1, "ERR CW MODE");
    CHECK(s_run == RUN_STOPPING);
    g_vfo_tx.Modulation = MODULATION_CW;
    run_for(1000, false);
    start_run(2, "TEST");
    g_vfo_tx.Modulation = MODULATION_FM;
    loop();
    // The CW engine is stopped whatever the mode now reads.
    CHECK(s_run == RUN_STOPPING && gCW_State == CW_INACTIVE && !g_reg30_tx);
}

// Time in runs is kept to a budget: a host sending CW after CW gets about half
// the time on the air.
static void test_the_key_down_budget(void)
{
    reset_radio();
    uint8_t id = 1;
    uint32_t keyed = 0;
    const uint32_t t0 = g_now;
    // A host that sends the next CW as soon as each ends, for ten minutes.
    while (g_now - t0 < 600000) {
        host_send(id, "CW 20 TEST");
        loop();
        uint8_t got;
        const char *r = next_reply(&got);
        if (!strcmp(r, "ERR CW DUTY")) {
            run_for(1000, false);
        } else {
            CHECK(r[0] == 0);
            run_for(10, false);
            key_down();
            loop();
            expect_reply(id, "OK CW");
            // 50 s of text, kept alive.
            for (int i = 0; i < 200; i++) {
                run_for(250, false);
                status_tx();
            }
            text_done();
            loop();
            keyed += 50000;
        }
        id = id == 0xFF ? 1 : id + 1;
    }
    // The first 165 s go out back to back; then about half the time.
    CHECK(keyed >= 300000 && keyed <= 400000);
    CHECK(s_duty_ms < HF_DUTY_REFUSE_MS + 60000);
}

// A run whose text went out without the BK4819 ever reading transmitting: the
// stop checks could not see a stuck transmitter, so no more CW.
static void test_a_transmitter_that_never_reads_on_refuses_cw(void)
{
    reset_radio();
    expect(1, "CW 20 E", "");
    run_for(10, false);
    gCW_State = CW_TRANSMITTING;   // the CW engine keyed, the BK4819 reads receive
    gCurrentFunction = FUNCTION_TRANSMIT;
    loop();
    expect_reply(1, "OK CW");
    for (int i = 0; i < 10; i++)
        loop();
    text_done();
    loop();
    CHECK(s_run == RUN_NONE && s_readback_bad);
    expect(2, "CW 20 TEST", "ERR CW CHECK");
    // Read on even once, it is fine.
    reset_radio();
    start_run(1, "E");
    g_reg30_tx = false;
    for (int i = 0; i < 10; i++)
        loop();
    text_done();
    loop();
    CHECK(s_run == RUN_NONE && !s_readback_bad);
}

static void test_the_watchdog(void)
{
    reset_radio();
    CHECK(g_wdg_enabled && s_wdg_started);
    // Outside a run: fed even with the main loop stopped.
    int fed = g_wdg_reloads;
    run_for(5000, true);
    CHECK(g_wdg_reloads == fed + 500);
    run_for(10, false);
    // In a run: fed while the main loop runs...
    start_run(1, "TEST");
    fed = g_wdg_reloads;
    for (int i = 0; i < 8; i++) {
        run_for(250, false);
        status_tx();
    }
    CHECK(g_wdg_reloads == fed + 200);
    // ...and not once it has stopped for a second.
    fed = g_wdg_reloads;
    run_for(3000, true);
    CHECK(g_wdg_reloads >= fed + 99 && g_wdg_reloads <= fed + 100);
}

static void test_the_hang_test(void)
{
    reset_radio();
    expect(1, "TEST HANG", "ERR TEST RUN");
    expect(2, "CW 20 TEST", "");
    expect(3, "TEST HANG", "ERR TEST RUN");   // not keyed yet
    CHECK(g_hangs == 0);
    key_down();
    loop();
    expect_reply(2, "OK CW");
    // The reply is given 20 ms to go out before the hang.
    g_usb_busy = true;
    g_auto_ms = 1;
    host_send(4, "TEST HANG");
    loop();
    g_auto_ms = 0;
    CHECK(g_hangs == 1);
    CHECK(g_usb_out[0] == 0);   // still held by the busy endpoint
    g_usb_busy = false;
    loop();
    expect_reply(4, "OK TEST HANG");
    expect(5, "TEST HANGS", "ERR TEST UNKNOWN");
}

static void test_quiet_time(void)
{
    reset_radio();
    run_for(1500, false);
    expect(1, "STATUS", "OK STATUS 0 1500" NOT_CHARGING);
    g_SquelchLost = true;
    run_for(10, false);
    expect(2, "STATUS", "OK STATUS 0 0" NOT_CHARGING);
    g_SquelchLost = false;
    run_for(700, false);
    expect(3, "STATUS", "OK STATUS 0 700" NOT_CHARGING);
    run_for(70000, true);
    expect(4, "STATUS", "OK STATUS 0 60000" NOT_CHARGING);
    // The squelch tail after our own transmission is not someone else.
    start_run(5, "TEST");
    text_done();
    loop();
    g_SquelchLost = true;
    run_for(200, false);
    g_SquelchLost = false;
    status_tx();
    run_for(200, false);
    expect(6, "STATUS", "OK STATUS 0 60000" NOT_CHARGING);
    g_SquelchLost = true;
    run_for(200, false);
    g_SquelchLost = false;
    run_for(100, false);
    expect(7, "STATUS", "OK STATUS 0 100" NOT_CHARGING);
}

// The radio must not transmit while it charges over USB-C: CW is refused, and a
// run is stopped, and its stop checked, like a STOP's.
static void test_charging(void)
{
    reset_radio();
    expect(1, "STATUS", "OK STATUS 0 0" NOT_CHARGING);
    gChargingWithTypeC = true;
    expect(2, "STATUS", "OK STATUS 0 0 1");
    expect(3, "CW 20 TEST", "ERR CW CHARGE");
    CHECK(g_played[0] == 0 && s_run == RUN_NONE && !s_run_armed);
    // Before the first key-down: the CW is answered.
    gChargingWithTypeC = false;
    expect(4, "CW 20 TEST", "");
    CHECK(s_run == RUN_STARTING);
    gChargingWithTypeC = true;
    loop();
    expect_reply(4, "ERR CW CHARGE");
    CHECK(s_run == RUN_STOPPING && s_run_armed && !gCW_PlaybackActive);
    CHECK(g_stop_playback_calls == 1);
    gChargingWithTypeC = false;
    expect(5, "CW 20 TEST", "ERR CW WAIT");
    run_for(1000, false);
    CHECK(s_run == RUN_NONE && !s_wdg_starve);
    // Keying: off at once, with no reply (the CW was answered), and watched.
    start_run(6, "TEST");
    gChargingWithTypeC = true;
    loop();
    CHECK(s_run == RUN_STOPPING && gCW_State == CW_INACTIVE && !g_reg30_tx);
    CHECK(g_usb_out[0] == 0);
    CHECK(!status_tx());
    run_for(1000, false);
    CHECK(s_run == RUN_NONE && !s_wdg_starve);
    expect(7, "CW 20 TEST", "ERR CW CHARGE");
    // A stop for charging that leaves the transmitter on resets the radio.
    gChargingWithTypeC = false;
    start_run(8, "TEST");
    g_end_tx_fails = true;
    gChargingWithTypeC = true;
    loop();
    CHECK(s_run == RUN_STOPPING && status_tx() && !s_wdg_starve);
    run_for(500, false);
    CHECK(s_wdg_starve);
}

static void test_replies_wait_for_the_host(void)
{
    reset_radio();
    g_usb_busy = true;
    for (uint8_t id = 1; id <= 6; id++)
        host_send(id, "BREAKIN");
    loop();
    CHECK(g_usb_out[0] == 0);
    g_usb_busy = false;
    loop();
    // Four held; the oldest dropped.
    for (uint8_t id = 3; id <= 6; id++)
        expect_reply(id, "OK BREAKIN 1");
    CHECK(g_usb_out[0] == 0);
}

int main(void)
{
    test_line_format();
    test_line_reader();
    test_line_parse();
    test_cw_chars();
    test_queries();
    test_damaged_lines_do_nothing();
    test_cw_refusals();
    test_a_run_ends_with_its_text();
    test_the_link_timeout_ends_a_run();
    test_the_transmit_limit_ends_a_run();
    test_a_cw_that_never_keys_is_refused();
    test_stop();
    test_a_stop_that_fails_resets_the_radio();
    test_repeated_stops_do_not_put_off_the_reset();
    test_a_held_paddle_after_a_stop_resets_the_radio();
    test_a_stop_by_a_limit_is_checked_too();
    test_switching_out_of_cw_ends_a_run();
    test_the_key_down_budget();
    test_a_transmitter_that_never_reads_on_refuses_cw();
    test_the_watchdog();
    test_the_hang_test();
    test_quiet_time();
    test_charging();
    test_replies_wait_for_the_host();
    if (g_failed) {
        fprintf(stderr, "%d check(s) failed\n", g_failed);
        return 1;
    }
    puts("all firmware host tests passed");
    return 0;
}
