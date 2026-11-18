// Astra WFP ALE_CONNECT_REDIRECT TCP callouts. The Agent supplies app-ID
// filters in a dynamic BFE session. This driver has no user-openable device.
#include <ntddk.h>
#include <fwpsk.h>

static const GUID provider_id =
    {0xf1759df4,0x58a7,0x484e,{0xb1,0xb0,0x2c,0xc2,0x66,0xd5,0x6b,0x43}};
static const GUID tcp_keys[] = {
    {0x2ee52311,0x055a,0x4c93,{0xbd,0x62,0x7b,0x06,0x47,0x17,0x52,0x3e}},
    {0x68cc4fb9,0xdf6b,0x4bfb,{0x91,0x4f,0x09,0xc2,0xfe,0x78,0x76,0xb9}}
};
static UINT32 callout_ids[2];
static HANDLE redirect_handle;

// Returned by SIO_QUERY_WFP_CONNECTION_REDIRECT_CONTEXT. Integers are
// little-endian except the network-order port. Address bytes are wire order.
typedef struct {
    UINT32 version;
    UINT32 pid;
    UINT16 family;
    UINT16 port;
    UINT32 scope;
    UCHAR address[16];
} OriginalTarget;
C_ASSERT(sizeof(OriginalTarget) == 32);

static void NTAPI classify_tcp(const FWPS_INCOMING_VALUES0 *values,
    const FWPS_INCOMING_METADATA_VALUES0 *meta, void *layer_data,
    const void *context, const FWPS_FILTER1 *filter, UINT64 flow,
    FWPS_CLASSIFY_OUT0 *result)
{
    UINT64 handle = 0;
    FWPS_CONNECT_REQUEST0 *request = NULL;
    OriginalTarget *target = NULL;
    const DWORD broker = (DWORD)(filter->context >> 32);
    const USHORT port = (USHORT)filter->context;
    const USHORT network_port = (USHORT)((port << 8) | (port >> 8));
    BOOLEAN permitted = FALSE;
    UNREFERENCED_PARAMETER(flow);
    if (!(result->rights & FWPS_RIGHT_ACTION_WRITE)) return;
    if (!broker || !port || !context || !layer_data ||
        !FWPS_IS_METADATA_FIELD_PRESENT(meta, FWPS_METADATA_FIELD_PROCESS_ID))
        goto done;
    if (meta->processId == broker) {
        permitted = TRUE;
        goto done;
    }
    if (FWPS_IS_METADATA_FIELD_PRESENT(meta, FWPS_METADATA_FIELD_REDIRECT_RECORD_HANDLE) &&
        meta->redirectRecords) {
        FWPS_CONNECTION_REDIRECT_STATE state =
            FwpsQueryConnectionRedirectState0(meta->redirectRecords, redirect_handle, NULL);
        if (state == FWPS_CONNECTION_REDIRECTED_BY_SELF) {
            permitted = TRUE;
            goto done;
        }
        if (state == FWPS_CONNECTION_PREVIOUSLY_REDIRECTED_BY_SELF)
            goto done;
    }
    // Allocate before requesting writable layer data. Once acquired, a
    // writable copy must always be returned to WFP through ApplyModified.
    target = ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(*target), 'rtSA');
    if (!target) goto done;
    if (!NT_SUCCESS(FwpsAcquireClassifyHandle0((void *)context, 0, &handle)))
        goto done;
    if (!NT_SUCCESS(FwpsAcquireWritableLayerDataPointer0(handle, filter->filterId, 0,
        (void **)&request, result)))
        goto done;
    target->version = 1;
    target->pid = (UINT32)meta->processId;
    target->family = request->remoteAddressAndPort.ss_family;
    if (values->layerId == FWPS_LAYER_ALE_CONNECT_REDIRECT_V4 && target->family == AF_INET) {
        SOCKADDR_IN *remote = (SOCKADDR_IN *)&request->remoteAddressAndPort;
        target->port = remote->sin_port;
        RtlCopyMemory(target->address, &remote->sin_addr, 4);
        remote->sin_addr.s_addr = 0;
        remote->sin_addr.S_un.S_un_b.s_b1 = 127;
        remote->sin_addr.S_un.S_un_b.s_b4 = 1;
        remote->sin_port = network_port;
    } else if (values->layerId == FWPS_LAYER_ALE_CONNECT_REDIRECT_V6 && target->family == AF_INET6) {
        SOCKADDR_IN6 *remote = (SOCKADDR_IN6 *)&request->remoteAddressAndPort;
        target->port = remote->sin6_port;
        target->scope = remote->sin6_scope_id;
        RtlCopyMemory(target->address, &remote->sin6_addr, 16);
        RtlZeroMemory(&remote->sin6_addr, 16);
        remote->sin6_addr.u.Byte[15] = 1;
        remote->sin6_scope_id = 0;
        remote->sin6_flowinfo = 0;
        remote->sin6_port = network_port;
    } else {
        goto done;
    }
    request->localRedirectTargetPID = broker;
    request->localRedirectHandle = redirect_handle;
    request->localRedirectContext = target;
    request->localRedirectContextSize = sizeof(*target);
    target = NULL; // WFP owns the redirect context after ApplyModified.
    permitted = TRUE;
done:
    if (request) FwpsApplyModifiedLayerData0(handle, request, 0);
    if (handle) FwpsReleaseClassifyHandle0(handle);
    if (target) ExFreePoolWithTag(target, 'rtSA');
    result->actionType = permitted ? FWP_ACTION_PERMIT : FWP_ACTION_BLOCK;
    if (!permitted || (filter->flags & FWPS_FILTER_FLAG_CLEAR_ACTION_RIGHT))
        result->rights &= ~FWPS_RIGHT_ACTION_WRITE;
    else
        result->rights |= FWPS_RIGHT_ACTION_WRITE;
}

static NTSTATUS NTAPI notify_filter(FWPS_CALLOUT_NOTIFY_TYPE type, const GUID *id, FWPS_FILTER1 *filter)
{
    UNREFERENCED_PARAMETER(type);
    UNREFERENCED_PARAMETER(id);
    UNREFERENCED_PARAMETER(filter);
    return STATUS_SUCCESS;
}

static void unload(PDRIVER_OBJECT driver)
{
    for (ULONG i = 0; i < RTL_NUMBER_OF(callout_ids); ++i) {
        if (callout_ids[i]) FwpsCalloutUnregisterById0(callout_ids[i]);
    }
    if (redirect_handle) FwpsRedirectHandleDestroy0(redirect_handle);
    if (driver->DeviceObject) IoDeleteDevice(driver->DeviceObject);
}

DRIVER_INITIALIZE DriverEntry;
NTSTATUS DriverEntry(PDRIVER_OBJECT driver, PUNICODE_STRING registry)
{
    UNREFERENCED_PARAMETER(registry);
    PDEVICE_OBJECT device = NULL;
    NTSTATUS status = IoCreateDevice(driver, 0, NULL, FILE_DEVICE_NETWORK, FILE_DEVICE_SECURE_OPEN,
        FALSE, &device);
    if (!NT_SUCCESS(status)) return status;
    status = FwpsRedirectHandleCreate0(&provider_id, 0, &redirect_handle);
    if (!NT_SUCCESS(status)) goto fail;
    for (ULONG i = 0; i < RTL_NUMBER_OF(callout_ids); ++i) {
        FWPS_CALLOUT1 callout = {0};
        callout.calloutKey = tcp_keys[i];
        callout.classifyFn = classify_tcp;
        callout.notifyFn = notify_filter;
        status = FwpsCalloutRegister1(device, &callout, &callout_ids[i]);
        if (!NT_SUCCESS(status)) goto fail;
    }
    driver->DriverUnload = unload;
    device->Flags &= ~DO_DEVICE_INITIALIZING;
    return STATUS_SUCCESS;
fail:
    unload(driver);
    return status;
}
