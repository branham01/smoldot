// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

import { ConnectionConfig, Connection, Config as SmoldotBindingsConfig, default as smoldotLightBindingsBuilder } from './bindings-smoldot-light.js';
import { Config as WasiConfig, default as wasiBindingsBuilder } from './bindings-wasi.js';

import { default as wasmBase64 } from './autogen/wasm.js';

import { SmoldotWasmInstance } from './bindings.js';

export { ConnectionConfig, ConnectionError, Connection } from './bindings-smoldot-light.js';

export interface Config {
    /**
     * Closure to call when the Wasm instance panics.
     *
     * This callback will always be invoked from within a binding called the Wasm instance.
     *
     * After this callback has been called, it is forbidden to invoke any function from the Wasm
     * VM.
     *
     * If this callback is called while invoking a function from the Wasm VM, this function will
     * throw a dummy exception.
     */
    onWasmPanic: (message: string) => void,
    logCallback: (level: number, target: string, message: string) => void,
    jsonRpcCallback: (response: string, chainId: number) => void,
    databaseContentCallback: (data: string, chainId: number) => void,
    currentTaskCallback?: (taskName: string | null) => void,
    cpuRateLimit: number,
}

/**
 * Contains functions that the client will use when it needs to leverage the platform.
 */
export interface PlatformBindings {
    /**
     * Base64-decode the given buffer then decompress its content using the inflate algorithm
     * with zlib header.
     *
     * The input is considered trusted. In other words, the implementation doesn't have to
     * resist malicious input.
     */
    base64DecodeAndZlibInflate: (input: string) => Uint8Array,

    /**
     * Returns the number of milliseconds since an arbitrary epoch.
     */
    performanceNow: () => number,

    /**
     * Fills the given buffer with randomly-generated bytes.
     */
    getRandomValues: (buffer: Uint8Array) => void,

    /**
     * Tries to open a new connection using the given configuration.
     *
     * @see Connection
     * @throws ConnectionError If the multiaddress couldn't be parsed or contains an invalid protocol.
     */
     connect(config: ConnectionConfig): Connection;
}

export async function startInstance(config: Config, platformBindings: PlatformBindings): Promise<SmoldotWasmInstance> {
    // The actual Wasm bytecode is base64-decoded then deflate-decoded from a constant found in a
    // different file.
    // This is suboptimal compared to using `instantiateStreaming`, but it is the most
    // cross-platform cross-bundler approach.
    const wasmBytecode = platformBindings.base64DecodeAndZlibInflate(wasmBase64)

    let killAll: () => void;

    // Used to bind with the smoldot-light bindings. See the `bindings-smoldot-light.js` file.
    const smoldotJsConfig: SmoldotBindingsConfig = {
        performanceNow: platformBindings.performanceNow,
        connect: platformBindings.connect,
        onPanic: (message) => {
            killAll();
            config.onWasmPanic(message);
            throw new Error();
        },
        ...config
    };

    // Used to bind with the Wasi bindings. See the `bindings-wasi.js` file.
    const wasiConfig: WasiConfig = {
        envVars: [],
        getRandomValues: platformBindings.getRandomValues,
        onProcExit: (retCode) => {
            killAll();
            config.onWasmPanic(`proc_exit called: ${retCode}`)
            throw new Error();
        }
    };

    const { imports: smoldotBindings, killAll: smoldotBindingsKillAll } =
        smoldotLightBindingsBuilder(smoldotJsConfig);

    killAll = smoldotBindingsKillAll;

    // Start the Wasm virtual machine.
    // The Rust code defines a list of imports that must be fulfilled by the environment. The second
    // parameter provides their implementations.
    const result = await WebAssembly.instantiate(wasmBytecode, {
        // The functions with the "smoldot" prefix are specific to smoldot.
        "smoldot": smoldotBindings,
        // As the Rust code is compiled for wasi, some more wasi-specific imports exist.
        "wasi_snapshot_preview1": wasiBindingsBuilder(wasiConfig),
    });

    const instance = result.instance as SmoldotWasmInstance;
    smoldotJsConfig.instance = instance;
    wasiConfig.instance = instance;
    return instance;
}
