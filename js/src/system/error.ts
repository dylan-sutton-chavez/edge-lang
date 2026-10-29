export class SystemError extends Error {
    readonly role: Role;

    constructor(role: Role, message: string) {
        super(message);
        this.role = role;
        this.name = role;
    }
}

// the runtime's descriptor names the class each role raises
export type Role = 'invalid' | 'denied' | 'failed';

/* a call whose arguments the module refuses before it reaches anything */
export const invalid = (message: string) => new SystemError('invalid', message);

/* a call no grant of the package covers */
export const denied = (message: string) => new SystemError('denied', message);

/* a call that reached out and met a failure */
export const failed = (message: string) => new SystemError('failed', message);
