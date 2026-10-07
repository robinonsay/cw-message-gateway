/* What hfnode.c uses of the NR7Y firmware, declared for a host test: every header
 * hfnode.c includes from the firmware is a stub here that includes this. The
 * definitions, which behave as the test needs, are in test_hfnode.c.
 *
 * Copyright 2026 the ic-7300-hf-server contributors
 * Licensed under the Apache License, Version 2.0 (see app/hfnode.c).
 */

#ifndef FAKE_RADIO_H
#define FAKE_RADIO_H

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

// external/printf/printf.h
#define sprintf_ sprintf

// functions.h
typedef enum {
    FUNCTION_FOREGROUND = 0,
    FUNCTION_TRANSMIT,
    FUNCTION_MONITOR,
    FUNCTION_INCOMING,
    FUNCTION_RECEIVE,
    FUNCTION_POWER_SAVE,
} FUNCTION_Type_t;
extern FUNCTION_Type_t gCurrentFunction;

// helper/battery.h
extern bool gChargingWithTypeC;

// misc.h, app/cwapp.h
enum { CW_INACTIVE = 0, CW_TRANSMITTING, CW_SUSPENDED };
extern uint8_t gCW_State;
extern bool g_SquelchLost;
void CW_EndTxNow(void);

// app/cwkeyer.h
extern bool gCW_PlaybackActive;
extern bool gCW_Recording;
extern uint8_t gCW_MessageRepeatCountdown_500ms;
bool CW_StartTextPlayback(const char *text, uint8_t wpm);
void CW_StopPlayback(void);

// radio.h
typedef enum {
    MODULATION_FM,
    MODULATION_AM,
    MODULATION_USB,
    MODULATION_CW,
    MODULATION_UKNOWN,
} ModulationMode_t;
enum {
    OUTPUT_POWER_USER,
    OUTPUT_POWER_LOW1,
    OUTPUT_POWER_LOW2,
    OUTPUT_POWER_LOW3,
    OUTPUT_POWER_LOW4,
    OUTPUT_POWER_LOW5,
    OUTPUT_POWER_MID,
    OUTPUT_POWER_HIGH,
};
typedef struct {
    uint32_t Frequency;   // 10 Hz units
} FREQ_Config_t;
typedef struct {
    FREQ_Config_t   *pRX;
    FREQ_Config_t   *pTX;
    ModulationMode_t Modulation;
    uint8_t          OUTPUT_POWER;
} VFO_Info_t;
extern VFO_Info_t *gTxVfo;
extern VFO_Info_t *gRxVfo;
extern const char gModulationStr[MODULATION_UKNOWN][4];
void RADIO_CW_Suspend(void);

// settings.h
typedef struct {
    bool CW_BREAKIN_ENABLE;
} EEPROM_Config_t;
extern EEPROM_Config_t gEeprom;

// driver/bk4819.h, driver/bk4819-regs.h
#define BK4819_REG_30               0x30
#define BK4819_REG_30_ENABLE_TX_DSP (1u << 1)
uint16_t BK4819_ReadRegister(uint8_t reg);

// driver/millis.h
uint32_t millis(void);
uint32_t millis_since(uint32_t prev);

// driver/vcp.h
#define VCP_RX_BUF_SIZE 256
extern uint8_t VCP_RxBuf[VCP_RX_BUF_SIZE];
extern volatile uint32_t VCP_RxBufPointer;

// usbd_core.h
int usbd_ep_start_write(uint8_t ep, const uint8_t *data, uint32_t len);

// py32f071_ll_iwdg.h, py32f071_ll_rcc.h
#define IWDG                  ((void *)0)
#define LL_IWDG_PRESCALER_32  3u
void     LL_IWDG_Enable(void *iwdg);
void     LL_IWDG_EnableWriteAccess(void *iwdg);
void     LL_IWDG_SetPrescaler(void *iwdg, uint32_t prescaler);
void     LL_IWDG_SetReloadCounter(void *iwdg, uint32_t counter);
uint32_t LL_IWDG_IsReady(void *iwdg);
void     LL_IWDG_ReloadCounter(void *iwdg);
void     LL_RCC_LSI_Enable(void);

#endif
