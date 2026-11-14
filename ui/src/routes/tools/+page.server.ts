// The Tools page's first read, made on this server so the page renders with
// it: through the same control proxy the browser uses, with the browser's
// Tailscale login, and never its cookies.
import { proxy } from '#lib/server/runsProxy.ts';
import { ToolsApi, type ToolListing } from '#lib/tools.ts';
import type { PageServerLoad } from './$types';

export type ToolsInitial =
  { kind: 'ready'; listing: ToolListing } | { kind: 'failed'; message: string };

export const load: PageServerLoad = async ({ request }) => {
  const login = request.headers.get('tailscale-user-login');
  const api = new ToolsApi((async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), request.url);
    // A GET from this server: no Origin, no cookies; the login as sent.
    return proxy.respond(
      new Request(url, {
        method: 'GET',
        headers: login === null ? {} : { 'tailscale-user-login': login },
        signal: init?.signal ?? null,
      }),
    );
  }) as typeof fetch);
  let initial: ToolsInitial;
  try {
    initial = { kind: 'ready', listing: await api.listing() };
  } catch (error) {
    initial = { kind: 'failed', message: error instanceof Error ? error.message : String(error) };
  }
  return { initial };
};
