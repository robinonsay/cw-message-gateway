/* hfnode serial control for the NR7Y CW firmware (UV-K5 v3 / UV-K1).
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

// The firmware side of hfnode's docs/handheld-protocol.md, over the USB-C virtual
// COM port only: the headset jack's serial line shares a wire with PTT, so bytes on
// it could key the radio.
//
// A "run" is one CW command: from its acceptance until its text has gone out (or it
// is stopped) and the transmitter reads off. During a run:
//   - it is stopped once it has lasted HF_TX_LIMIT_S;
//   - it is stopped once no valid line has arrived for HF_LINK_TIMEOUT_MS, so a
//     crashed or killed hfnode, or a pulled cable, ends it;
//   - a stop, by hfnode's STOP or by one of these limits, is checked: if the
//     transmitter still reads on HF_STOP_GRACE_MS later, the watchdog is no longer
//     fed, and resets the radio;
//   - the watchdog resets the radio if the main loop stops for HF_WDG_STALE_MS,
//     since a hang would leave the keyer, and maybe the carrier, where they were.
// A reset turns the transmitter off: the start-up code resets the BK4819 (main.c,
// BK4819_Init) within milliseconds. The radio's own transmit time-out timer does not
// run in CW (cwapp.c clears it on every key-down), so these are the firmware's only
// limits.

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "app/hfnode.h"
#include "app/hfnode_line.h"
#include "app/cwapp.h"
#include "app/cwkeyer.h"
#include "app/cwmacro.h"
#ifdef ENABLE_CODE_PRACTICE
#include "app/cpo.h"
#endif
#include "driver/bk4819.h"
#include "driver/bk4819-regs.h"
#include "driver/millis.h"
#include "driver/vcp.h"
#include "external/printf/printf.h"
#include "functions.h"
#include "misc.h"
#include "py32f071_ll_iwdg.h"
#include "py32f071_ll_rcc.h"
#include "radio.h"
#include "settings.h"
#include "usbd_core.h"

#define HF_VERSION            1
#define HF_TX_LIMIT_S         60
#define HF_LINK_TIMEOUT_MS    2000
// From a CW being accepted to the first key-down: the keyer starts at once, so
// longer than this means the radio refused to transmit.
#define HF_KEY_START_MS       300
// From a stop to the transmitter reading off; the keyer's own break-in tail is
// shorter (cw_suspend_limit, 300 ms), and a stop cuts it short anyway.
#define HF_STOP_GRACE_MS      500
#define HF_WDG_STALE_MS       1000
// The watchdog counts LSI (about 32 kHz) / 32: 2048 counts is about 2 s.
#define HF_WDG_RELOAD         2048
#define HF_QUIET_MAX_MS       60000
// The receiver's own squelch tail after a transmission is not someone else.
#define HF_SQUELCH_HOLDOFF_MS 300
#define HF_NAME               "NR7Y-CW HFNODE"
#define HF_WPM_MIN            5
#define HF_WPM_MAX            50
#define HF_TEXT_MAX           30

// The CDC IN endpoint (CDC_IN_EP in usb/usbd_cdc_if.c).
#define HF_CDC_IN_EP          0x81
// One full-speed packet: every reply fits in one, so a write needs no buffer kept
// for the interrupt and no zero-length packet after it.
#define HF_REPLY_CAP          64
#define HF_TXQ                4

// The hang of the TEST HANG command. A test build defines it to return.
#ifndef HF_HANG
#define HF_HANG() for (;;) {}
#endif

typedef enum {
    RUN_NONE = 0,
    RUN_STARTING,   // accepted, waiting for the first key-down to reply
    RUN_KEYING,     // replied; until the text is out and the transmitter is off
    RUN_STOPPING,   // stopped; until the transmitter reads off
} HF_Run_t;

static HF_LineReader_t s_reader;
static uint32_t        s_rx_read;

static char    s_txq[HF_TXQ][HF_REPLY_CAP];
static uint8_t s_txq_len[HF_TXQ];
static uint8_t s_txq_head;
static uint8_t s_txq_count;

static HF_Run_t s_run;
static uint8_t  s_run_id;        // the CW command's id, for its deferred reply
static uint32_t s_run_start_ms;
static uint32_t s_stop_ms;       // when RUN_STOPPING began
static uint32_t s_last_line_ms;

static uint32_t s_squelch_open_ms;   // last time the squelch was seen open
static uint32_t s_tx_off_ms;         // last time the transmitter was seen on

static volatile bool     s_wdg_started;
static volatile bool     s_run_armed;
static volatile bool     s_wdg_starve;   // a stop failed: let the watchdog reset the radio
static volatile uint32_t s_loop_beat_ms;

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

static void hf_flush(void)
{
    while (s_txq_count > 0) {
        const int r = usbd_ep_start_write(HF_CDC_IN_EP, (const uint8_t *)s_txq[s_txq_head],
                                          s_txq_len[s_txq_head]);
        if (r == -3)
            return;   // the last packet has not been read yet: try again next pass
        // Sent, or no host to send to: either way it is done with.
        s_txq_head = (s_txq_head + 1) % HF_TXQ;
        s_txq_count--;
    }
}

static void hf_reply(uint8_t id, const char *body)
{
    if (s_txq_count == HF_TXQ) {
        // Nobody is reading: the oldest reply is the least use.
        s_txq_head = (s_txq_head + 1) % HF_TXQ;
        s_txq_count--;
    }
    const uint8_t slot = (s_txq_head + s_txq_count) % HF_TXQ;
    const size_t len = HF_LineFormat(s_txq[slot], HF_REPLY_CAP, id, body);
    if (len == 0)
        return;
    s_txq_len[slot] = (uint8_t)len;
    s_txq_count++;
    hf_flush();
}

static void hf_error(uint8_t id, const char *cmd, const char *code)
{
    char body[HF_REPLY_CAP];
    // A command name that is not one of ours is cut short to fit.
    sprintf_(body, "ERR %.20s %s", cmd, code);
    hf_reply(id, body);
}

// ---------------------------------------------------------------------------
// Radio state
// ---------------------------------------------------------------------------

// What the firmware has keyed, from its own state.
static bool hf_tx_soft(void)
{
    return gCurrentFunction == FUNCTION_TRANSMIT || gCW_State != CW_INACTIVE;
}

// The transmitter's state: the firmware's, and the BK4819's transmit DSP, which is
// on whenever the chip is set up to transmit (RX_TurnOn clears it).
static bool hf_tx_on(void)
{
    if (hf_tx_soft())
        return true;
    return (BK4819_ReadRegister(BK4819_REG_30) & BK4819_REG_30_ENABLE_TX_DSP) != 0;
}

static bool hf_busy(void)
{
    if (s_run != RUN_NONE || hf_tx_soft() || gCW_PlaybackActive || gCW_Recording
        || gCW_MessageRepeatCountdown_500ms > 0)
        return true;
#ifdef ENABLE_CODE_PRACTICE
    if (gCW_CpoActive)
        return true;
#endif
    return false;
}

// Stop whatever the CW engine is sending and return to receive. A transmission in
// another mode (FM with the PTT held) is the operator's, and left alone.
static void hf_stop(void)
{
    if (gCW_PlaybackActive || gCW_MessageRepeatCountdown_500ms > 0)
        CW_StopPlayback();
    // The CW engine's own state first, whatever the mode reads now.
    if (gCW_State == CW_TRANSMITTING)
        RADIO_CW_Suspend();
    if (gCW_State != CW_INACTIVE
        || (gTxVfo->Modulation == MODULATION_CW && gCurrentFunction == FUNCTION_TRANSMIT))
        CW_EndTxNow();
}

static void hf_end_run(void)
{
    s_run = RUN_NONE;
    s_run_armed = false;
}

// Stop the run, and keep it until the transmitter reads off (hf_run_limits).
static void hf_halt(void)
{
    hf_stop();
    if (!hf_tx_on()) {
        hf_end_run();
        return;
    }
    s_run = RUN_STOPPING;
    s_stop_ms = millis();
    s_run_armed = true;
}

static const char *hf_power_name(uint8_t p)
{
    static const char *const names[] = {
        [OUTPUT_POWER_USER] = "USER", [OUTPUT_POWER_LOW1] = "LOW1",
        [OUTPUT_POWER_LOW2] = "LOW2", [OUTPUT_POWER_LOW3] = "LOW3",
        [OUTPUT_POWER_LOW4] = "LOW4", [OUTPUT_POWER_LOW5] = "LOW5",
        [OUTPUT_POWER_MID] = "MID",   [OUTPUT_POWER_HIGH] = "HIGH",
    };
    return p < sizeof(names) / sizeof(names[0]) && names[p] ? names[p] : "OTHER";
}

static const char *hf_mode_name(uint8_t m)
{
    return m < MODULATION_UKNOWN && gModulationStr[m][0] ? gModulationStr[m] : "OTHER";
}

static uint32_t hf_quiet_ms(void)
{
    const uint32_t q = millis_since(s_squelch_open_ms);
    return q > HF_QUIET_MAX_MS ? HF_QUIET_MAX_MS : q;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

static bool hf_parse_wpm(const char *s, uint8_t *wpm)
{
    unsigned v = 0;
    if (!*s || strlen(s) > 2)
        return false;
    for (; *s; s++) {
        if (*s < '0' || *s > '9')
            return false;
        v = v * 10 + (unsigned)(*s - '0');
    }
    *wpm = (uint8_t)v;
    return true;
}

static void hf_cw(uint8_t id, char *arg)
{
    char *text = strchr(arg, ' ');
    uint8_t wpm = 0;
    if (!text) {
        hf_error(id, "CW", "LEN");
        return;
    }
    *text++ = 0;
    if (!hf_parse_wpm(arg, &wpm) || wpm < HF_WPM_MIN || wpm > HF_WPM_MAX) {
        hf_error(id, "CW", "WPM");
        return;
    }
    const size_t len = strlen(text);
    if (len == 0 || len > HF_TEXT_MAX) {
        hf_error(id, "CW", "LEN");
        return;
    }
    for (size_t i = 0; i < len; i++) {
        if (!HF_CwCharOk(text[i])) {
            hf_error(id, "CW", "CHAR");
            return;
        }
    }
    if (gTxVfo->Modulation != MODULATION_CW) {
        hf_error(id, "CW", "MODE");
        return;
    }
    // Without break-in the keyer only plays the sidetone.
    if (!gEeprom.CW_BREAKIN_ENABLE) {
        hf_error(id, "CW", "BKIN");
        return;
    }
    if (hf_busy() || !CW_StartTextPlayback(text, wpm)) {
        hf_error(id, "CW", "TX");
        return;
    }
    s_run = RUN_STARTING;
    s_run_id = id;
    s_run_start_ms = millis();
    s_run_armed = true;
}

static void hf_command(uint8_t id, char *body)
{
    char out[HF_REPLY_CAP];
    char *arg = strchr(body, ' ');
    if (arg)
        *arg++ = 0;

    if (!strcmp(body, "HELLO") && !arg) {
        sprintf_(out, "OK HELLO %u %u %u %s", HF_VERSION, HF_TX_LIMIT_S, HF_LINK_TIMEOUT_MS,
                 HF_NAME);
        hf_reply(id, out);
    } else if (!strcmp(body, "STATUS") && !arg) {
        const bool tx = s_run != RUN_NONE || hf_tx_on();
        sprintf_(out, "OK STATUS %u %u", tx ? 1u : 0u, (unsigned)hf_quiet_ms());
        hf_reply(id, out);
    } else if (!strcmp(body, "FREQ") && !arg) {
        // Frequencies are kept in 10 Hz units.
        sprintf_(out, "OK FREQ %u %u", (unsigned)(gRxVfo->pRX->Frequency * 10u),
                 (unsigned)(gTxVfo->pTX->Frequency * 10u));
        hf_reply(id, out);
    } else if (!strcmp(body, "MODE") && !arg) {
        sprintf_(out, "OK MODE %s %s", hf_mode_name(gTxVfo->Modulation),
                 hf_mode_name(gRxVfo->Modulation));
        hf_reply(id, out);
    } else if (!strcmp(body, "POWER") && !arg) {
        sprintf_(out, "OK POWER %s", hf_power_name(gTxVfo->OUTPUT_POWER));
        hf_reply(id, out);
    } else if (!strcmp(body, "BREAKIN") && !arg) {
        sprintf_(out, "OK BREAKIN %u", gEeprom.CW_BREAKIN_ENABLE ? 1u : 0u);
        hf_reply(id, out);
    } else if (!strcmp(body, "STOP") && !arg) {
        // The reply says the stop was made; STATUS reads tx 1 until the transmitter
        // is off.
        if (s_run == RUN_STARTING)
            hf_error(s_run_id, "CW", "STOP");
        if (s_run == RUN_NONE)
            hf_stop();   // the CW engine keyed by hand, say: not checked
        else
            hf_halt();
        hf_reply(id, "OK STOP");
    } else if (!strcmp(body, "CW") && arg) {
        hf_cw(id, arg);
    } else if (!strcmp(body, "TEST") && arg && !strcmp(arg, "HANG")) {
        // Bring-up only: proves the watchdog by stopping the main loop during a run.
        // Refused outside a run, where the watchdog would let the hang go on.
        if (s_run != RUN_KEYING) {
            hf_error(id, "TEST", "RUN");
            return;
        }
        hf_reply(id, "OK TEST HANG");
        const uint32_t t0 = millis();
        // Let the reply go out, then stop.
        while (s_txq_count > 0 && millis_since(t0) < 20)
            hf_flush();
        HF_HANG();
    } else {
        hf_error(id, body, "UNKNOWN");
    }
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

static void hf_read(void)
{
    // Written by the USB interrupt; it can briefly read VCP_RX_BUF_SIZE, which is
    // the same place as 0.
    uint32_t end = VCP_RxBufPointer;
    if (end >= VCP_RX_BUF_SIZE)
        end = 0;
    uint32_t n = 0;
    while (s_rx_read != end && n++ < VCP_RX_BUF_SIZE) {
        const uint8_t b = VCP_RxBuf[s_rx_read];
        s_rx_read = (s_rx_read + 1) % VCP_RX_BUF_SIZE;
        if (!HF_LineFeed(&s_reader, b))
            continue;
        uint8_t id;
        char *body;
        // A damaged line is not a line: no reply, and it does not keep the link.
        if (HF_LineParse(s_reader.buf, &id, &body)) {
            s_last_line_ms = millis();
            hf_command(id, body);
        }
    }
}

static void hf_run_limits(void)
{
    if (s_run == RUN_NONE)
        return;
    if (s_run == RUN_STOPPING) {
        if (!hf_tx_on()) {
            hf_end_run();
            return;
        }
        hf_stop();
        if (millis_since(s_stop_ms) >= HF_STOP_GRACE_MS)
            s_wdg_starve = true;
        return;
    }
    if (gTxVfo->Modulation != MODULATION_CW) {
        // Switched out of CW by hand: the keyer no longer runs.
        if (s_run == RUN_STARTING)
            hf_error(s_run_id, "CW", "MODE");
        hf_halt();
        return;
    }
    if (millis_since(s_run_start_ms) >= HF_TX_LIMIT_S * 1000u
        || millis_since(s_last_line_ms) >= HF_LINK_TIMEOUT_MS) {
        if (s_run == RUN_STARTING)
            hf_error(s_run_id, "CW", "TX");
        hf_halt();
        return;
    }
    if (s_run == RUN_STARTING) {
        // Keyed: transmitting, or already in the break-in tail of a first element.
        if (gCW_State != CW_INACTIVE) {
            hf_reply(s_run_id, "OK CW");
            s_run = RUN_KEYING;
        } else if (!gCW_PlaybackActive || millis_since(s_run_start_ms) >= HF_KEY_START_MS) {
            // The radio refused to transmit (TX lock, frequency, battery), or a
            // paddle or key stopped the playback before it began.
            hf_error(s_run_id, "CW", "REFUSED");
            hf_halt();
        }
        return;
    }
    // Keying: over once the text is out and the transmitter is off again.
    if (!gCW_PlaybackActive && !hf_tx_on())
        hf_end_run();
}

static void hf_squelch(void)
{
    const uint32_t now = millis();
    if (hf_tx_soft()) {
        s_tx_off_ms = now;
        return;
    }
    if (g_SquelchLost && millis_since(s_tx_off_ms) >= HF_SQUELCH_HOLDOFF_MS)
        s_squelch_open_ms = now;
}

void HFNODE_Init(void)
{
    const uint32_t now = millis();
    s_last_line_ms = now;
    s_squelch_open_ms = now;
    s_tx_off_ms = now;
    s_loop_beat_ms = now;
    // Read only what arrives from now on.
    s_rx_read = VCP_RxBufPointer >= VCP_RX_BUF_SIZE ? 0 : VCP_RxBufPointer;

    LL_RCC_LSI_Enable();
    LL_IWDG_Enable(IWDG);
    LL_IWDG_EnableWriteAccess(IWDG);
    LL_IWDG_SetPrescaler(IWDG, LL_IWDG_PRESCALER_32);
    LL_IWDG_SetReloadCounter(IWDG, HF_WDG_RELOAD);
    while (!LL_IWDG_IsReady(IWDG)) {
    }
    LL_IWDG_ReloadCounter(IWDG);
    s_wdg_started = true;
}

void HFNODE_Poll(void)
{
    s_loop_beat_ms = millis();
    hf_read();
    hf_run_limits();
    hf_squelch();
    hf_flush();
}

void HFNODE_WatchdogTick(void)
{
    if (!s_wdg_started || s_wdg_starve)
        return;
    // Outside a run the watchdog is always fed (a hang there is no worse than
    // before); a hard fault stops this interrupt too, and so resets the radio.
    if (!s_run_armed || millis_since(s_loop_beat_ms) < HF_WDG_STALE_MS)
        LL_IWDG_ReloadCounter(IWDG);
}
