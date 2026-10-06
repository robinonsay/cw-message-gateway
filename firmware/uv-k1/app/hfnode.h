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

// Lets hfnode key the radio over the USB-C virtual COM port, with the command set
// of hfnode's docs/handheld-protocol.md, and with the limits that make that safe to
// leave to a computer: a transmit limit per run, a link timeout, a check of every
// stop, a key-down budget, and a hardware watchdog that resets the radio if its
// main loop stops during a run or a stop fails.

#ifndef APP_HFNODE_H
#define APP_HFNODE_H

// Once, just before the main loop: starts the watchdog.
void HFNODE_Init(void);

// On every pass of the main loop: reads commands, answers them, and enforces the
// limits on a run.
void HFNODE_Poll(void);

// From SysTick, every 10 ms: counts hfnode's own clock, and feeds the watchdog
// unless a run is on and the main loop has stopped, or a stop has failed.
void HFNODE_WatchdogTick(void);

#endif
