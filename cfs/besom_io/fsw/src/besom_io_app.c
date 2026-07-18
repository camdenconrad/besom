/************************************************************************
 * besom_io -- the sensor bridge.
 *
 * Closes the loop. Without this, the flight software is flying blind: cFS's lab
 * apps emit housekeeping into the void and never learn where the spacecraft
 * actually is. besom_io receives simulated vehicle state from the Besom harness
 * and publishes it on the software bus, so flight apps consume spacecraft state
 * the way they would in orbit -- and the ground sees state that has round-tripped
 * THROUGH the flight software, not just Besom's own copy of it.
 *
 * Determinism, which is the whole point of the harness, is preserved by *how*
 * this app is driven:
 *
 *   - It wakes on an OSAL timer bound to "cFS-Master" -- the timebase Besom
 *     steps. So it runs on simulated time, not wall time.
 *   - It reads the sensor block from the PSP, inside the timer callback, on the
 *     timebase thread. The block was installed by that same thread before
 *     simulated time advanced, so the sample it reads is the one belonging to
 *     the tick being processed -- by construction, not by timing.
 *
 * HISTORY, because the previous design's failure is worth keeping. State used to
 * arrive as its own UDP datagram, one per tick at 100 Hz, which this app drained
 * to the newest sample on its own 10 Hz cycle. Draining to the newest was itself
 * a fix: reading one datagram per cycle consumed the queue slower than it filled,
 * so the backlog grew without bound and published state fell steadily further
 * into the past.
 *
 * But draining to the newest only moved the problem. How many datagrams the
 * kernel had delivered by the time this app looked was a host decision, so the
 * app latched a sample one full 10 Hz cycle off between otherwise identical runs
 * -- an exact 0.1 s offset in published position. Every check passed, because
 * each run was internally consistent and nothing compared payload contents
 * across runs.
 *
 * The queue is gone rather than better managed. There is no socket, no backlog,
 * and no arrival timing left to depend on.
 ************************************************************************/

#include "cfe.h"
#include "cfe_psp_besom.h"
#include "besom_io_app.h"

BESOM_IO_Data_t BESOM_IO_Data;

/*
 * THE MECHANISM.
 *
 * Runs on the timebase thread, dispatched for the tick whose sensor block the
 * PSP has just installed. Reading here -- rather than signalling the task and
 * reading there -- is what makes the published sample a function of simulated
 * time: on this thread the read is ordered against the PSP's write by program
 * order, and the step protocol serialises the next tick behind the previous
 * acknowledgement, so no other tick can be in flight. Off this thread the
 * accessor refuses (see CFE_PSP_Besom_GetSensorBlock), so the invariant is
 * enforced rather than remembered.
 *
 * A callback must not block or transmit, so it latches and gives the semaphore;
 * the task publishes.
 */
static void BESOM_IO_TimerCallback(osal_id_t object_id, void *arg)
{
    BESOM_IO_Sample_t sample;
    uint32            size = 0;
    uint32            seq  = 0;
    int32             status;

    /*
     * Ring full: the task has not kept up. Count it, give NO semaphore, and do
     * NOT advance Write -- so the semaphore count and the number of unread
     * latches stay exactly equal and the k-th publish still carries the k-th
     * accepted firing. The cost is one missing 0x08F0 packet, which moves tick
     * placement, which `besomctl check` already detects and reports. A loud,
     * counted failure in preference to a silently overwritten sample.
     */
    if (BESOM_IO_Data.Write - BESOM_IO_Data.Read >= BESOM_IO_RING)
    {
        ++BESOM_IO_Data.HkTlm.Payload.OverrunCount;
        return;
    }

    status = CFE_PSP_Besom_GetSensorBlock(&sample, sizeof(sample), &size, &seq);

    if (status != CFE_PSP_SUCCESS || size != sizeof(sample))
    {
        /* Republish the previous latch rather than a torn or absent one. */
        ++BESOM_IO_Data.HkTlm.Payload.RxErrCount;
        sample = BESOM_IO_Data.Ring[(BESOM_IO_Data.Write - 1) & (BESOM_IO_RING - 1)];
    }
    else if (seq == BESOM_IO_Data.LastSeq)
    {
        /* The harness granted a tick without new state. Not an error; visible. */
        ++BESOM_IO_Data.HkTlm.Payload.StaleCount;
    }
    else
    {
        BESOM_IO_Data.LastSeq = seq;
        ++BESOM_IO_Data.HkTlm.Payload.RxCount;
    }

    BESOM_IO_Data.Ring[BESOM_IO_Data.Write & (BESOM_IO_RING - 1)] = sample;
    ++BESOM_IO_Data.Write;

    OS_CountSemGive(BESOM_IO_Data.TimingSem);
}

static int32 BESOM_IO_Init(void)
{
    int32     status;
    osal_id_t timebase_id;

    memset(&BESOM_IO_Data, 0, sizeof(BESOM_IO_Data));
    BESOM_IO_Data.RunStatus = CFE_ES_RunStatus_APP_RUN;

    status = CFE_EVS_Register(NULL, 0, CFE_EVS_EventFilter_BINARY);
    if (status != CFE_SUCCESS)
    {
        return status;
    }

    CFE_MSG_Init(CFE_MSG_PTR(BESOM_IO_Data.HkTlm.TelemetryHeader),
                 CFE_SB_ValueToMsgId(BESOM_IO_STATE_TLM_MID), sizeof(BESOM_IO_Data.HkTlm));

    /* ---- run on SIMULATED time ---- */
    status = OS_CountSemCreate(&BESOM_IO_Data.TimingSem, "BESOM_IO_SEM", 0, 0);
    if (status != OS_SUCCESS)
    {
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

    /* "cFS-Master" is the timebase the PSP owns -- under Besom, the one the
     * harness steps. Hanging our timer here is what puts this app on the
     * simulated clock rather than the host's. */
    status = OS_TimeBaseGetIdByName(&timebase_id, "cFS-Master");
    if (status != OS_SUCCESS)
    {
        CFE_ES_WriteToSysLog("BESOM_IO: no cFS-Master timebase: %ld\n", (long)status);
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

    status = OS_TimerAdd(&BESOM_IO_Data.TimerId, "BESOM_IO", timebase_id, BESOM_IO_TimerCallback,
                         NULL);
    if (status != OS_SUCCESS)
    {
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

    status = OS_TimerSet(BESOM_IO_Data.TimerId, BESOM_IO_RATE_USEC, BESOM_IO_RATE_USEC);
    if (status != OS_SUCCESS)
    {
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

    CFE_EVS_SendEvent(BESOM_IO_INIT_EID, CFE_EVS_EventType_INFORMATION,
                      "BESOM_IO initialized: vehicle state from the timebase, publishing 0x%04X "
                      "at %d Hz",
                      BESOM_IO_STATE_TLM_MID, 1000000 / BESOM_IO_RATE_USEC);

    return CFE_SUCCESS;
}

void BESOM_IO_AppMain(void)
{
    int32 status;

    CFE_ES_PerfLogEntry(BESOM_IO_PERF_ID);

    if (BESOM_IO_Init() != CFE_SUCCESS)
    {
        BESOM_IO_Data.RunStatus = CFE_ES_RunStatus_APP_ERROR;
    }

    while (CFE_ES_RunLoop(&BESOM_IO_Data.RunStatus) == true)
    {
        CFE_ES_PerfLogExit(BESOM_IO_PERF_ID);

        /* Pend on simulated time. */
        status = OS_CountSemTake(BESOM_IO_Data.TimingSem);

        CFE_ES_PerfLogEntry(BESOM_IO_PERF_ID);

        if (status != OS_SUCCESS)
        {
            BESOM_IO_Data.RunStatus = CFE_ES_RunStatus_APP_ERROR;
            break;
        }

        /* Take the latch this semaphore give corresponds to. Read advances in
         * lockstep with Write, so this is the sample latched by the k-th
         * accepted firing -- not "the newest", which is what used to make the
         * published value depend on scheduling. */
        BESOM_IO_Data.HkTlm.Payload.Sample =
            BESOM_IO_Data.Ring[BESOM_IO_Data.Read & (BESOM_IO_RING - 1)];
        ++BESOM_IO_Data.Read;

        /* Publish every cycle, whether or not new state arrived -- consumers of
         * spacecraft state want it at a steady rate, and a gap in the downlink
         * would be indistinguishable from a dropped packet on the ground. */
        CFE_SB_TimeStampMsg(CFE_MSG_PTR(BESOM_IO_Data.HkTlm.TelemetryHeader));
        CFE_SB_TransmitMsg(CFE_MSG_PTR(BESOM_IO_Data.HkTlm.TelemetryHeader), true);
    }

    CFE_ES_ExitApp(BESOM_IO_Data.RunStatus);
}
