/**
 * Sessions through the UI server: the one place the UI changes something
 * on the daemon. It is off unless the operator names the daemon's control
 * token file (`REN_CONTROL_TOKEN_FILE`), and it reaches only the daemon's
 * `/control/v1/sessions` routes (listing, creating, prompting, answering,
 * configuring, interrupting, stopping, and archiving sessions, their
 * composer's commands and files, and their update feed), its
 * `/control/v1/runs` reads (the live runs and their feed, the projects a
 * session can start in, and the clients and models it can start with), and
 * its `/control/v1/history` routes (archiving, unarchiving, and deleting a
 * recording or a run), and its `/control/v1/tools` routes (the host's tools
 * for agents: their listing, a change, and a collection). Never inference,
 * reads of history, the operator state's session blocks, or the rest of the
 * control API.
 *
 * The browser never sees the token: this server reads it from the file on
 * each request (the daemon writes a new one when it starts) and sends it as
 * the bearer. A browser request must come from this UI's own pages (same
 * origin, JSON body), and with `REN_CONTROL_LOGINS` set, from one of those
 * Tailscale logins, as Tailscale Serve reports them.
 */

/** Who may drive runs through this UI, parsed once from the environment. */
export type RunsAccess =
  | { kind: 'off' }
  | {
      kind: 'on';
      /** The daemon's control token file, read on every request. */
      tokenFile: string;
      /**
       * Tailscale logins allowed; empty means anyone who reaches the UI,
       * for a UI kept on this host's loopback.
       */
      logins: readonly string[];
    };

export class RunsAccessConfig {
  /** `REN_CONTROL_TOKEN_FILE`: absent, or an absolute path. */
  static tokenFile(value: string | undefined): string | undefined {
    if (value === undefined || value === '') return undefined;
    if (!value.startsWith('/')) throw new Error('REN_CONTROL_TOKEN_FILE must be an absolute path.');
    return value;
  }

  /** `REN_CONTROL_LOGINS`: comma-separated Tailscale logins. */
  static logins(value: string | undefined): string[] {
    const listed = (value ?? '')
      .split(',')
      .map((login) => login.trim())
      .filter((login) => login !== '');
    for (const login of listed) {
      if (login.length > 256 || !/^[\x21-\x7e]+$/.test(login)) {
        throw new Error('REN_CONTROL_LOGINS entries must be 1 to 256 visible characters.');
      }
    }
    return listed;
  }

  /** Both variables, already parsed. Logins without a token file are a mistake. */
  static access(tokenFile: string | undefined, logins: readonly string[]): RunsAccess {
    if (tokenFile === undefined) {
      if (logins.length > 0) throw new Error('REN_CONTROL_LOGINS needs REN_CONTROL_TOKEN_FILE.');
      return { kind: 'off' };
    }
    return { kind: 'on', tokenFile, logins };
  }
}

/** What the proxy needs from its host: the network and the token file. */
export interface ControlHost {
  fetch: typeof fetch;
  readToken(path: string): Promise<string>;
}

/**
 * `query`: the query parameters forwarded; every other one is dropped.
 * `timeout`: how long a request that is not a feed may take, in ms.
 */
type Route = {
  method: 'GET' | 'POST';
  feed: boolean;
  query?: readonly string[];
  timeout?: number;
};

const runId = '[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}';
/** Approval ids are the client's own; kept to URL-safe text as browsers encode it. */
const approvalId = "[A-Za-z0-9._~%:!$&'()*+,;=@-]{1,256}";
const routes: { pattern: RegExp; route: Route }[] = [
  { pattern: /^\/control\/runs$/, route: { method: 'GET', feed: false } },
  { pattern: /^\/control\/runs\/events$/, route: { method: 'GET', feed: true } },
  { pattern: /^\/control\/runs\/projects$/, route: { method: 'GET', feed: false } },
  { pattern: /^\/control\/runs\/clients$/, route: { method: 'GET', feed: false } },
  {
    pattern: /^\/control\/runs\/clients\/(claude_code|codex|pi|opencode)\/models$/,
    route: { method: 'GET', feed: false },
  },
  // Sessions: their listing and feed, one session, and what it is offered.
  {
    pattern: /^\/control\/sessions$/,
    route: { method: 'GET', feed: false, query: ['shelf', 'limit'] },
  },
  { pattern: /^\/control\/sessions\/events$/, route: { method: 'GET', feed: true } },
  { pattern: new RegExp(`^/control/sessions/${runId}$`), route: { method: 'GET', feed: false } },
  {
    pattern: new RegExp(`^/control/sessions/${runId}/commands$`),
    route: { method: 'GET', feed: false },
  },
  {
    pattern: new RegExp(`^/control/sessions/${runId}/files$`),
    route: { method: 'GET', feed: false, query: ['q', 'limit'] },
  },
  {
    pattern: new RegExp(
      `^/control/sessions/${runId}/(?:prompt|unqueue|settings|client|interrupt|stop|archive|unarchive|delete)$`,
    ),
    route: { method: 'POST', feed: false },
  },
  {
    pattern: new RegExp(`^/control/sessions/${runId}/approvals/${approvalId}$`),
    route: { method: 'POST', feed: false },
  },
  {
    pattern: new RegExp(
      `^/control/history/(?:sessions|runs)/${runId}/(?:archive|unarchive|delete)$`,
    ),
    route: { method: 'POST', feed: false },
  },
  // Tools: the listing reads versions by running each program found, and a
  // change downloads and unpacks releases, so both may take a while.
  { pattern: /^\/control\/tools$/, route: { method: 'GET', feed: false, timeout: 60_000 } },
  {
    pattern: /^\/control\/tools\/change$/,
    route: { method: 'POST', feed: false, timeout: 15 * 60_000 },
  },
  { pattern: /^\/control\/tools\/gc$/, route: { method: 'POST', feed: false, timeout: 60_000 } },
];
/** Creating a session is the one POST to the session listing's own path. */
const sessionsPath = '/control/sessions';
const maxBody = 256 * 1024;
const headers = { 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' };
/** Sent first on the feed, so EventSource reports it open at once (see management.ts). */
const openingComment = new TextEncoder().encode(': connected\n\n');

export class RunsProxy {
  readonly #access: RunsAccess;
  readonly #origin: string;
  readonly #host: ControlHost;

  constructor(access: RunsAccess, daemonOrigin: string, host: ControlHost) {
    this.#access = access;
    this.#origin = daemonOrigin;
    this.#host = host;
  }

  async respond(request: Request): Promise<Response> {
    const access = this.#access;
    if (access.kind === 'off') return RunsProxy.#refuse(404, 'runs_off');
    const url = new URL(request.url);
    const route = RunsProxy.#route(url.pathname, request.method);
    if (route === null) return new Response(null, { status: 404, headers });
    if (route === 'method') {
      return new Response(null, { status: 405, headers });
    }
    if (!RunsProxy.#sameOrigin(request, url)) return RunsProxy.#refuse(403, 'cross_origin');
    if (access.logins.length > 0) {
      const login = request.headers.get('tailscale-user-login');
      if (login === null || !access.logins.includes(login)) {
        return RunsProxy.#refuse(403, 'login_not_allowed');
      }
    }
    let body: string | undefined;
    if (route.method === 'POST') {
      if (request.headers.get('content-type')?.split(';')[0]?.trim() !== 'application/json') {
        return RunsProxy.#refuse(415, 'json_required');
      }
      body = await request.text();
      if (body.length > maxBody) return RunsProxy.#refuse(413, 'too_large');
    }
    let token: string;
    try {
      token = (await this.#host.readToken(access.tokenFile)).trim();
    } catch {
      return RunsProxy.#refuse(503, 'no_control_token');
    }
    if (token === '') return RunsProxy.#refuse(503, 'no_control_token');
    const target = new URL(url.pathname.replace(/^\/control\//, '/control/v1/'), this.#origin);
    for (const name of route.query ?? []) {
      const value = url.searchParams.get(name);
      if (value !== null) target.searchParams.set(name, value);
    }
    const timeout = AbortSignal.timeout(route.timeout ?? 30_000);
    try {
      // Global fetch: no browser cookies, credentials, or Origin are forwarded.
      const upstream = await this.#host.fetch(target, {
        method: route.method,
        headers: {
          authorization: `Bearer ${token}`,
          accept: route.feed ? 'text/event-stream' : 'application/json',
          ...(body === undefined ? {} : { 'content-type': 'application/json' }),
        },
        body,
        redirect: 'manual',
        signal: route.feed ? request.signal : AbortSignal.any([request.signal, timeout]),
      });
      if (upstream.status >= 300 && upstream.status < 400) {
        await upstream.body?.cancel();
        return RunsProxy.#refuse(502, 'unexpected_redirect');
      }
      const streamed =
        route.feed && upstream.ok && upstream.body ? RunsProxy.#feed(upstream.body) : upstream.body;
      return new Response(streamed, {
        status: upstream.status,
        headers: {
          ...headers,
          'content-type':
            upstream.headers.get('content-type') ??
            (route.feed ? 'text/event-stream' : 'application/json'),
        },
      });
    } catch {
      return RunsProxy.#refuse(!route.feed && timeout.aborted ? 504 : 502, 'daemon_unreachable');
    }
  }

  static #route(path: string, method: string): Route | 'method' | null {
    if (path === sessionsPath && method === 'POST') return { method: 'POST', feed: false };
    const found = routes.find(({ pattern }) => pattern.test(path));
    if (!found) return null;
    return found.route.method === method ? found.route : 'method';
  }

  /**
   * Only this UI's own pages. Browsers send `Sec-Fetch-Site`, which pages
   * cannot set; without it, a POST's `Origin` must name this host. Only the
   * host is compared: behind Tailscale Serve, which ends TLS, this server
   * cannot tell the scheme the browser used. A POST with neither header did
   * not come from a browser page.
   */
  static #sameOrigin(request: Request, url: URL): boolean {
    const site = request.headers.get('sec-fetch-site');
    if (site !== null) return site === 'same-origin';
    const origin = request.headers.get('origin');
    if (origin === null) return request.method === 'GET';
    try {
      return new URL(origin).host === url.host;
    } catch {
      return false;
    }
  }

  static #refuse(status: number, error: string): Response {
    return Response.json({ error }, { status, headers });
  }

  static #feed(upstream: ReadableStream<Uint8Array>): ReadableStream<Uint8Array> {
    const reader = upstream.getReader();
    return new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(openingComment);
      },
      async pull(controller) {
        const { done, value } = await reader.read();
        if (done) controller.close();
        else controller.enqueue(value);
      },
      cancel(reason) {
        return reader.cancel(reason);
      },
    });
  }
}
