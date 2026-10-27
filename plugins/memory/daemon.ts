// Shared memory daemon: one process (embedder, sqlite, Bun runtime) serving every mu session over a unix socket.
// `mu-memory` (the Rust shim, plugins/memory-shim) starts it on demand and relays each session's stdio to it.
// Every connection is an independent plugin session, so `core/complete` calls go back over the same connection.
import { chmodSync, unlinkSync } from "node:fs";
import * as net from "node:net";
import { log, serve } from "./rpc";
import { MemorySession } from "./session";

/** Sent on accept so a client can tell a live daemon from a socket left behind by one that is exiting. */
export const READY_LINE = "mu-memory ready\n";

export async function runDaemon(socketPath: string, idleMs: number): Promise<void> {
	const sessions = new Set<MemorySession>();
	let idleTimer: Timer | undefined;
	let stopping = false;

	const stop = (code: number) => {
		stopping = true;
		server.close();
		try {
			unlinkSync(socketPath);
		} catch {
			// already gone
		}
		process.exit(code);
	};

	// No connection for `idleMs`: exit. Everything here is synchronous, so a connection accepted a tick
	// earlier has already cleared the timer and one arriving later is refused and retried by the shim.
	const armIdle = () => {
		clearTimeout(idleTimer);
		idleTimer = setTimeout(() => {
			if (sessions.size === 0) {
				log(`no sessions for ${Math.round(idleMs / 1000)}s; exiting`);
				stop(0);
			}
		}, idleMs);
	};

	const server = net.createServer(socket => {
		if (stopping) return void socket.destroy();
		clearTimeout(idleTimer);
		const session = new MemorySession();
		sessions.add(session);
		socket.on("error", () => {});
		socket.write(READY_LINE);
		const write = (line: string) => {
			if (!socket.destroyed && socket.writable) socket.write(line);
		};
		void serve(session.handle, { input: socket, write })
			.catch(e => log(`connection failed: ${e instanceof Error ? e.message : String(e)}`))
			.then(() => session.endSession())
			.catch(e => log(`session end failed: ${e instanceof Error ? e.message : String(e)}`))
			.finally(() => {
				sessions.delete(session);
				socket.end();
				if (sessions.size === 0) armIdle();
			});
	});
	server.on("error", e => {
		log(`server error: ${e.message}`);
		process.exit(1);
	});

	// The shim only starts a daemon when the old socket is dead and holds its lock, so any file here is stale.
	try {
		unlinkSync(socketPath);
	} catch {
		// no stale socket
	}
	await new Promise<void>(resolve => server.listen(socketPath, resolve));
	chmodSync(socketPath, 0o600);
	armIdle();
	log(`daemon ${process.pid} listening on ${socketPath}`);

	const shutdown = async () => {
		if (stopping) return;
		stopping = true;
		await Promise.allSettled([...sessions].map(s => s.endSession()));
		stop(0);
	};
	process.on("SIGTERM", () => void shutdown());
	process.on("SIGINT", () => void shutdown());
}
