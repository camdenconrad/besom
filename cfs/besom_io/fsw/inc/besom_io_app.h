/************************************************************************
 * besom_io -- interface definitions.
 ************************************************************************/

#ifndef BESOM_IO_APP_H
#define BESOM_IO_APP_H

#include "cfe.h"

/*
 * Message ids are raw values rather than mission topic-ids: this app is a
 * simulation bridge, not flight software, and wiring it into the mission's
 * topic-id tables would put simulation-only identifiers into the flight config.
 * 0x08F0 is unused by the cFS bundle.
 */
#define BESOM_IO_STATE_TLM_MID 0x08F0

/* Where the harness sends vehicle state. */
#define BESOM_IO_STATE_PORT 5010

/* 10 Hz of SIMULATED time (the timebase Besom steps, not the host clock). */
#define BESOM_IO_RATE_USEC 100000

#define BESOM_IO_PERF_ID 0x60
#define BESOM_IO_INIT_EID 1

/*
 * Vehicle state, exactly as it goes on the wire from Besom.
 *
 * Little-endian doubles, native layout. This is a host-to-host simulation link,
 * not a spacecraft downlink, so it does not carry CCSDS framing or byte-order
 * conversion -- adding either would be ceremony that buys nothing and could
 * silently disagree with the sender.
 */
typedef struct
{
    double PosKm[3];  /**< Earth-centred inertial position, km */
    double VelKmS[3]; /**< Earth-centred inertial velocity, km/s */
    double AltKm;     /**< Altitude above the ellipsoid, km */
    double LatDeg;    /**< Sub-satellite latitude */
    double LonDeg;    /**< Sub-satellite longitude (inertial) */
    double Roll;      /**< Body roll about nadir, radians */
} BESOM_IO_State_t;

typedef struct
{
    BESOM_IO_State_t State;
    uint32           RxCount;    /**< state datagrams accepted */
    uint32           RxErrCount; /**< malformed datagrams seen */
} BESOM_IO_Payload_t;

typedef struct
{
    CFE_MSG_TelemetryHeader_t TelemetryHeader;
    BESOM_IO_Payload_t        Payload;
} BESOM_IO_StateTlm_t;

typedef struct
{
    BESOM_IO_StateTlm_t HkTlm;
    uint32              RunStatus;
    osal_id_t           TimerId;
    osal_id_t           TimingSem;
    osal_id_t           SockId;
    OS_SockAddr_t       SockAddr;
} BESOM_IO_Data_t;

extern BESOM_IO_Data_t BESOM_IO_Data;

void BESOM_IO_AppMain(void);

#endif /* BESOM_IO_APP_H */
