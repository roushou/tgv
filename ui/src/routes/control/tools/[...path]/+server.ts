import { proxy } from '#lib/server/runsProxy.ts';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = ({ request }) => proxy.respond(request);
export const POST: RequestHandler = ({ request }) => proxy.respond(request);
