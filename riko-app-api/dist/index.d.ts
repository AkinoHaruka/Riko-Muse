import type { IncomingMessage, ServerResponse } from "node:http";
export declare const name = "riko-app-api";
export declare const inject: string[];
export interface Config {
    apiTokenFile: string;
    sessionRegistryFile: string;
}
interface SessionControllerLike {
    list(request: Record<string, never>, signal: AbortSignal): Promise<{
        items: SessionSummaryLike[];
    }>;
    create(request: {
        sessionId: string;
        agentPreset: string;
    }): Promise<{
        sessionId: string;
        agentPreset?: string;
    }>;
    modelCatalog(): Promise<unknown>;
    selectModel(request: {
        sessionId: string;
        provider: string;
        model: string;
        reasoningEffort?: string;
    }): Promise<unknown>;
    page(request: {
        address: {
            kind: "session";
            sessionId: string;
        };
        throughSeq: number;
        beforeSeq: number;
        maxMessages: number;
    }, signal: AbortSignal): Promise<{
        records: readonly unknown[];
        hasMore: boolean;
    }>;
    prompt(request: {
        requestId: string;
        sessionId: string;
        mode: "queue" | "steer";
        content: readonly {
            type: "text";
            text: string;
        }[];
    }, signal: AbortSignal): Promise<unknown>;
    cancel(request: {
        sessionId: string;
    }): unknown;
    follow(request: {
        address: {
            kind: "session";
            sessionId: string;
        };
        assistantStream?: true;
        maxMessages?: number;
    }, signal: AbortSignal): AsyncIterable<unknown>;
}
interface SettingsNamespaceLike {
    ns: string;
    value: unknown;
    user?: unknown;
    revision: number;
}
interface SettingsControllerLike {
    writable: boolean;
    describe(options?: {
        redactSecrets?: boolean;
    }): SettingsNamespaceLike[];
    mutate(ns: string, ops: Array<{
        op: "set";
        path: string[];
        value: unknown;
    } | {
        op: "unset";
        path: string[];
    }>, expectedRevision?: number): Promise<void>;
}
interface CredentialControllerLike {
    describe(ref: string): Promise<{
        configured: boolean;
        writable: boolean;
    }>;
    set(ref: string, value: string): Promise<void>;
    unset(ref: string): Promise<void>;
}
interface LlmProviderLike {
    id: string;
    name: string;
}
interface ConfigurableProviderLike {
    provider: string;
    displayName: string;
    settingsNs: string;
    settingsPath: string[];
    active?: boolean;
    declared?: boolean;
    error?: string;
}
interface LlmRegistryLike {
    listProviders(): LlmProviderLike[];
    listConfigurableProviders(): ConfigurableProviderLike[];
    discoverModels(settingsNs: string, request: {
        provider?: string;
        baseURL?: string;
        api?: string;
        apiKey?: string;
    }): Promise<unknown[]>;
}
interface SessionSummaryLike {
    sessionId: string;
    updatedAt: number;
    running: boolean;
    blank: boolean;
}
interface WebServerLike {
    register(route: {
        kind: "prefix";
        path: string;
        handler: (req: IncomingMessage, res: ServerResponse) => void | Promise<void>;
    }): () => void;
}
interface MobileContext {
    webServer: WebServerLike;
    sessionController: SessionControllerLike;
    settings: SettingsControllerLike;
    credentials: CredentialControllerLike;
    llm: LlmRegistryLike;
    effect(effect: () => void | (() => void)): void;
    logger?: {
        warn(message: string): void;
        error(message: string): void;
        info(message: string): void;
    };
}
export declare function apply(ctx: MobileContext, rawConfig: Config): void;
export {};
