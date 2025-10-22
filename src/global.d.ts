export {};

declare global {
  namespace NodeJS {
    interface ProcessEnv {
      [key: string]: string | undefined;
    }

    interface Process {
      env: ProcessEnv;
      on(event: "SIGINT" | "SIGTERM", listener: () => void): this;
      exit(code?: number): never;
    }

    interface Timeout {
      ref(): this;
      unref(): this;
    }
  }

  const process: NodeJS.Process;

  function setInterval(
    callback: (...args: unknown[]) => void,
    ms?: number,
    ...args: unknown[]
  ): NodeJS.Timeout;

  function clearInterval(interval?: NodeJS.Timeout): void;

  interface Console {
    log: (...data: unknown[]) => void;
    info: (...data: unknown[]) => void;
    error: (...data: unknown[]) => void;
    warn: (...data: unknown[]) => void;
  }

  const console: Console;
}
