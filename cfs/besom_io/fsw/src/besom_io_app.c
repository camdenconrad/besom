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
 *   - It reads its socket with OS_CHECK (non-blocking). A blocking or timed read
 *     would pend on the host clock and drag the app back out of the simulation,
 *     which is exactly the failure mode that made cFS's HS app irreproducible.
 *
 * Besom sends one state datagram per granted tick and then waits for quiescence,
 * so the data is always there when this app looks.
 ************************************************************************/

#include "cfe.h"
#include "besom_io_app.h"

BESOM_IO_Data_t BESOM_IO_Data;

/* Woken by the timebase Besom steps. Gives the sem; the task does the work --
 * a timer callback must not block or transmit. */
static void BESOM_IO_TimerCallback(osal_id_t object_id, void *arg)
{
    OS_CountSemGive(BESOM_IO_Data.TimingSem);
}

/*
 * Pull one state datagram, if the harness left us one.
 *
 * Non-blocking by design: see the file header. Returns true if state was updated.
 */
static bool BESOM_IO_Poll(void)
{
    BESOM_IO_State_t state;
    OS_SockAddr_t    from;
    bool             got = false;

    /*
     * DRAIN to the newest sample, do not take the oldest.
     *
     * The harness sends state every simulated tick (100 Hz), but this app wakes
     * at 10 Hz, so several datagrams are queued each cycle. Reading only one per
     * cycle means consuming the queue slower than it fills: the socket backlog
     * grows without bound and the state published to the software bus falls
     * steadily further into the past. It looks like a plausible small lag and it
     * is in fact unbounded drift -- caught only because the ground station
     * displays flight-software state against the harness's own, and the position
     * error kept growing.
     *
     * A sensor reports what is true NOW. Discard the backlog and keep the last.
     */
    /*
     * A short or oversized datagram must be COUNTED and SKIPPED, not treated as
     * end-of-queue.  Testing `== sizeof(state)` in the loop condition made one
     * malformed datagram end the drain, leaving every valid sample queued behind
     * it -- the backlog the comment above exists to prevent, reintroduced by the
     * error path.  It also left RxErrCount permanently 0, so `besomctl loop`'s
     * "(0 malformed)" was structurally true rather than evidence of anything.
     *
     * Only a non-positive return means "nothing left to read".
     */
    int32 n;

    while ((n = OS_SocketRecvFrom(BESOM_IO_Data.SockId, &state, sizeof(state), &from, OS_CHECK)) > 0)
    {
        if (n != (int32)sizeof(state))
        {
            ++BESOM_IO_Data.HkTlm.Payload.RxErrCount;
            continue;
        }

        BESOM_IO_Data.HkTlm.Payload.State = state;
        ++BESOM_IO_Data.HkTlm.Payload.RxCount;
        got = true;
    }

    return got;
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

    /* ---- the link to the harness ---- */
    status = OS_SocketOpen(&BESOM_IO_Data.SockId, OS_SocketDomain_INET, OS_SocketType_DATAGRAM);
    if (status != OS_SUCCESS)
    {
        CFE_ES_WriteToSysLog("BESOM_IO: OS_SocketOpen failed: %ld\n", (long)status);
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

    OS_SocketAddrInit(&BESOM_IO_Data.SockAddr, OS_SocketDomain_INET);
    OS_SocketAddrSetPort(&BESOM_IO_Data.SockAddr, BESOM_IO_STATE_PORT);

    status = OS_SocketBind(BESOM_IO_Data.SockId, &BESOM_IO_Data.SockAddr);
    if (status != OS_SUCCESS)
    {
        CFE_ES_WriteToSysLog("BESOM_IO: OS_SocketBind(%d) failed: %ld\n", BESOM_IO_STATE_PORT,
                             (long)status);
        return CFE_STATUS_EXTERNAL_RESOURCE_FAIL;
    }

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
                      "BESOM_IO initialized: vehicle state on UDP %d, publishing 0x%04X at %d Hz",
                      BESOM_IO_STATE_PORT, BESOM_IO_STATE_TLM_MID, 1000000 / BESOM_IO_RATE_USEC);

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

        BESOM_IO_Poll();

        /* Publish every cycle, whether or not new state arrived -- consumers of
         * spacecraft state want it at a steady rate, and a gap in the downlink
         * would be indistinguishable from a dropped packet on the ground. */
        CFE_SB_TimeStampMsg(CFE_MSG_PTR(BESOM_IO_Data.HkTlm.TelemetryHeader));
        CFE_SB_TransmitMsg(CFE_MSG_PTR(BESOM_IO_Data.HkTlm.TelemetryHeader), true);
    }

    CFE_ES_ExitApp(BESOM_IO_Data.RunStatus);
}
