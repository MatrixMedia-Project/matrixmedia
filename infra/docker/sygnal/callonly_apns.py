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

from typing import List

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
