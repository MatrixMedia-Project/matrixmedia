import { lazy, type ComponentType, type LazyExoticComponent } from 'react';
import { Overview } from '../pages/Overview';

export type PageComponent = LazyExoticComponent<ComponentType> | ComponentType;

/**
 * The page component of every route, keyed by RouteDef.id (src/routes/manifest.ts). Overview is
 * eager so the first load ships only the shell and Overview; every other page is fetched on first
 * navigation. manifest.test.ts asserts the keys equal the manifest ids.
 */
export const PAGES: Record<string, PageComponent> = {
  overview: Overview,
  streams: lazy(() => import('../pages/Streams').then((m) => ({ default: m.Streams }))),
  'broadcast-servers': lazy(() =>
    import('../pages/broadcast-servers/BroadcastServersPage').then((m) => ({ default: m.BroadcastServersPage })),
  ),
  recordings: lazy(() => import('../pages/Recordings').then((m) => ({ default: m.Recordings }))),
  donations: lazy(() => import('../pages/Donations').then((m) => ({ default: m.Donations }))),
  subscriptions: lazy(() => import('../pages/Subscriptions').then((m) => ({ default: m.Subscriptions }))),
  'content-gates': lazy(() => import('../pages/ContentGates').then((m) => ({ default: m.ContentGates }))),
  ads: lazy(() => import('../pages/Ads').then((m) => ({ default: m.Ads }))),
  'payments-sandbox': lazy(() =>
    import('../pages/payments-sandbox/PaymentsSandbox').then((m) => ({ default: m.PaymentsSandbox })),
  ),
  users: lazy(() => import('../pages/Users').then((m) => ({ default: m.Users }))),
  creators: lazy(() => import('../pages/Creators').then((m) => ({ default: m.Creators }))),
  moderation: lazy(() => import('../pages/Moderation').then((m) => ({ default: m.Moderation }))),
  settings: lazy(() => import('../pages/settings/SettingsPage').then((m) => ({ default: m.SettingsPage }))),
  logs: lazy(() => import('../pages/Logs').then((m) => ({ default: m.Logs }))),
  'switch-lab': lazy(() => import('../pages/SwitchLab').then((m) => ({ default: m.SwitchLab }))),
  analytics: lazy(() => import('../pages/Analytics').then((m) => ({ default: m.Analytics }))),
  'server-requests': lazy(() => import('../pages/ServerRequests').then((m) => ({ default: m.ServerRequests }))),
  'stream-detail': lazy(() => import('../pages/StreamDetail').then((m) => ({ default: m.StreamDetail }))),
  'request-server': lazy(() => import('../pages/RequestServer').then((m) => ({ default: m.RequestServer }))),
  'creator-home': lazy(() => import('../pages/CreatorHome').then((m) => ({ default: m.CreatorHome }))),
  'creator-earnings': lazy(() => import('../pages/MyEarnings').then((m) => ({ default: m.MyEarnings }))),
  'creator-analytics': lazy(() => import('../pages/MyAnalytics').then((m) => ({ default: m.MyAnalytics }))),
  'creator-tiers': lazy(() => import('../pages/MyTiers').then((m) => ({ default: m.MyTiers }))),
  'creator-subscribers': lazy(() => import('../pages/MySubscribers').then((m) => ({ default: m.MySubscribers }))),
  'creator-profile': lazy(() => import('../pages/MyProfile').then((m) => ({ default: m.MyProfile }))),
  'creator-defaults': lazy(() => import('../pages/MyDefaults').then((m) => ({ default: m.MyDefaults }))),
  'creator-rooms': lazy(() => import('../pages/MyRooms').then((m) => ({ default: m.MyRooms }))),
};
