declare module "node:http" {
  class Agent {
    constructor(options?: { localAddress?: string });
  }

  export { Agent };
}

declare module "node:https" {
  class Agent {
    constructor(options?: { localAddress?: string });
  }

  export { Agent };
}
