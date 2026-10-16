import { globalHandlersIntegration, init } from '@sentry/browser';
import { dsn } from './sentry-dsn.json';

const origin = 'https://cantrip.mistystep.io';
const release = document.querySelector<HTMLMetaElement>('meta[name="cantrip-site-release"]')?.content;
let sent = 0;

// The public site has no account, dictation input, or analytics. Keep only code
// locations and classes: even an exception message can contain private content.
init({
  dsn,
  release,
  environment: location.origin === origin ? 'production' : 'development',
  defaultIntegrations: false,
  integrations: [globalHandlersIntegration()],
  dataCollection: {
    userInfo: false, cookies: false, httpHeaders: false, httpBodies: [],
    urlQueryParams: false, stackFrameVariables: false, frameContextLines: 0,
  },
  beforeSendLog: () => null,
  beforeSendMetric: () => null,
  sendClientReports: false,
  maxBreadcrumbs: 0,
  tracesSampleRate: 0,
  beforeSend(event) {
    if (sent >= 3) return null;
    const values = event.exception?.values?.map((exception) => ({
      type: /^(Error|TypeError|ReferenceError|SyntaxError|RangeError|URIError|EvalError|AggregateError)$/.test(exception.type ?? '')
        ? exception.type : 'Error',
      value: 'Unhandled website exception',
      mechanism: exception.mechanism && {
        type: exception.mechanism.type,
        handled: exception.mechanism.handled,
      },
      stacktrace: {
        frames: (exception.stacktrace?.frames ?? []).filter((frame) => {
          try {
            const url = new URL(frame.filename ?? '');
            return url.origin === origin && /^\/_astro\/[\w.-]+\.js$/.test(url.pathname);
          } catch { return false; }
        }).map((frame) => ({
          filename: new URL(frame.filename!).origin + new URL(frame.filename!).pathname,
          lineno: frame.lineno,
          colno: frame.colno,
        })),
      },
    }));
    if (!values?.length) return null;
    sent += 1;
    // Allowlist rather than a growing denylist of SDK/browser context fields.
    return {
      type: undefined,
      event_id: event.event_id,
      timestamp: event.timestamp,
      platform: 'javascript',
      level: 'error',
      release,
      environment: location.origin === origin ? 'production' : 'development',
      exception: { values },
      tags: { surface: 'website' },
    };
  },
});
