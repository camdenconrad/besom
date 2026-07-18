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

/* 10 Hz of SIMULATED time (the timebase Besom steps, not the host clock). */
#define BESOM_IO_RATE_USEC 100000

/*
 * Latches held between the timer callback (which reads the sensor block) and the
 * app task (which publishes it). Four is generous: the task is normally one
 * cycle behind at most, and a full ring is a counted, visible failure rather
 * than a silent overwrite -- see OverrunCount.
 */
#define BESOM_IO_RING 4

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

/*
 * One sensor reading: the state, and the simulated instant at which it is true.
 *
 * SampleUsec travels verbatim from the harness to telemetry -- no arithmetic on
 * either side. The ground can therefore check *which* sample was published, not
 * merely that some plausible-looking state arrived, which is the difference that
 * made a one-cycle sampling offset invisible for so long.
 *
 * Byte-identical to the block the harness puts on the wire, so the app copies it
 * straight into telemetry with no repacking.
 */
typedef struct
{
    uint64           SampleUsec; /**< simulated usec at which State is true */
    BESOM_IO_State_t State;
} BESOM_IO_Sample_t;

typedef struct
{
    BESOM_IO_Sample_t Sample;
    uint32            RxCount;       /**< firings that carried a NEW sensor block */
    uint32            RxErrCount;    /**< accessor failures / wrong-sized blocks */
    uint32            StaleCount;    /**< firings where the harness supplied nothing new */
    uint32            OverrunCount;  /**< firings dropped because the task had not kept up */
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

    /*
     * Written by the timer callback, read by the app task. Write advances only
     * when a latch is actually stored, and the callback gives the semaphore
     * exactly once per stored latch -- so the semaphore count and the number of
     * unread latches stay equal, and the k-th publish always carries the k-th
     * accepted firing however the task is scheduled.
     */
    BESOM_IO_Sample_t Ring[BESOM_IO_RING];
    volatile uint32   Write;
    uint32            Read;
    uint32            LastSeq; /**< sensor_seq of the last block accepted */
} BESOM_IO_Data_t;

extern BESOM_IO_Data_t BESOM_IO_Data;

void BESOM_IO_AppMain(void);

#endif /* BESOM_IO_APP_H */
