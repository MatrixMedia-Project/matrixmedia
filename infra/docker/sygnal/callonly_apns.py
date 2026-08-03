# MatrixMedia — call-only APNs pushkin for the iOS VoIP/PushKit app_ids.
#
# Why this exists
# ---------------
# Synapse fans EVERY notifiable event to ALL of an account's pushers,
# and stock Sygnal has no per-app event-type filter. iOS mandates a
# CallKit report within ~5s of every PushKit (voip) wake — so if the
# `.voip` app_id receives a plain message, the device briefly flashes a
# fake incoming call. Messages are handled entirely by the regular
# `event_id_only` pusher + the app's Notification Service Extension
# (FluffyChat model); the `.voip` pusher must therefore ring ONLY for
# real calls.
#
# This subclass drops (no-op, no APNs send) any notification whose
# Matrix event type is not a call type. Call events delegate to the
# stock ApnsPushkin unchanged. Wire it via sygnal.yaml:
#
#     com.steegler.matrixmedia.prod.prod.voip:
#       type: callonly_apns.CallOnlyApnsPushkin
#       certfile: ...
#       platform: production
#       topic: com.steegler.matrixmedia.prod.voip
#       push_type: voip
#
# Lives on PYTHONPATH via infra/docker/Dockerfile.sygnal. Override point
# `_dispatch_notification_unlimited(self, n, device, context) -> List[str]`
# is the per-(notification, device) entry; returning [] = no rejected
# pushkeys = silently not sent.

from typing import Any, Dict, List, Optional

from sygnal.apnspushkin import ApnsPushkin
from sygnal.notifications import Device, Notification, NotificationContext

CALL_EVENT_TYPES = frozenset(
    {
        "m.call.invite",
        "m.call.notify",
        "m.rtc.notification",
        "org.matrix.msc4075.rtc.notification",
        "org.matrix.msc4075.call.notify",
    }
)


class CallOnlyApnsPushkin(ApnsPushkin):
    """ApnsPushkin that only forwards call events (everything else is
    dropped before it can wake PushKit and force a CallKit report)."""

    async def _dispatch_notification_unlimited(
        self, n: Notification, device: Device, context: NotificationContext
    ) -> List[str]:
        if n.type not in CALL_EVENT_TYPES:
            # Not a call → do not send to the VoIP/PushKit app_id.
            # Empty list == zero rejected pushkeys == clean no-op.
            # (The regular pusher + NSE handle this event.)
            return []
        return await super()._dispatch_notification_unlimited(n, device, context)

    # ------------------------------------------------------------------
    # SILENT VoIP payload (the killed-state-ring fix).
    #
    # A PushKit (`apns-push-type: voip`) push MUST be silent — it carries
    # NO `aps.alert`. Its sole job is to wake the app so it can report a
    # CallKit incoming call. The stock `_get_payload_full` puts every
    # MSC4075 RTC type into its catch-all `elif n.type:` branch and stamps
    # an `aps.alert` with `loc-key MSG_FROM_USER` ("You have a new
    # message"). On a KILLED device iOS then renders that alert as a plain
    # notification and never cold-launches PushKit → no CallKit ring (and a
    # bogus "message" banner). On an alive device PushKit still delivers,
    # which is why the bug only showed when the app was force-quit.
    #
    # We therefore build a data-only payload (room_id / event_id + caller
    # hints, no `aps`) for the `.voip` pusher — mirroring the
    # `event_id_only` shape but keeping the caller fields the app's
    # PushKit handler (`CallServiceImpl.resolveCallerName`) reads so the
    # CallKit UI shows a real name. Non-call events never reach here (they
    # are dropped in `_dispatch_notification_unlimited` above).
    # ------------------------------------------------------------------
    def _get_payload_full(
        self, n: Notification, device: Device, log: Any, send_badge_counts: bool
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {}

        default_payload: Optional[Dict[str, Any]] = None
        if device.data:
            default_payload = device.data.get("default_payload")
        if isinstance(default_payload, dict):
            payload.update(default_payload)

        # CRITICAL: strip any `aps` the device registered in its
        # `default_payload` (the iOS SDK seeds one with an
        # "You have a new message" alert). A PushKit VoIP push with ANY
        # `aps.alert` is rendered by iOS as a plain notification on a
        # killed device instead of cold-launching for CallKit — which is
        # exactly the "killed → no ring" bug. The push MUST be data-only.
        payload.pop("aps", None)

        if n.room_id:
            payload["room_id"] = n.room_id
        if n.event_id:
            payload["event_id"] = n.event_id
        # Caller hints (optional, present only on full-format pushers) so
        # CallKit can show a name instead of the generic "Incoming call".
        if getattr(n, "room_name", None):
            payload["room_name"] = n.room_name
        if getattr(n, "sender_display_name", None):
            payload["caller_name"] = n.sender_display_name
        if getattr(n, "sender", None):
            payload["caller_id"] = n.sender

        # Deliberately NO `aps` key → silent VoIP wake.
        return payload
