// The ordered `controls/input` track (protocol::Input): sequence-numbered
// JSON events, a new group every 256 events, a heartbeat (`ping`) every
// 500 ms so the host's 3 s input timeout never fires while the session is
// open. Each frame is sealed for its group before it is written.

export interface InputEvent {
  type: string;
  [key: string]: unknown;
}

export interface GroupWriter {
  /** Sequence number of the group, as the track assigned it. */
  readonly sequence: number;
  write(payload: Uint8Array): void;
  close(): void;
}

export class InputWriter {
  #sequence = 0;
  #group: GroupWriter | undefined;
  readonly #append: () => GroupWriter;
  readonly #seal: (group: number, plaintext: Uint8Array) => Uint8Array;

  constructor(append: () => GroupWriter, seal: (group: number, plaintext: Uint8Array) => Uint8Array) {
    this.#append = append;
    this.#seal = seal;
  }

  /** Events written so far (the next event's sequence number). */
  get sequence(): number {
    return this.#sequence;
  }

  send(event: InputEvent): void {
    if (!this.#group || (this.#sequence > 0 && this.#sequence % 256 === 0)) {
      this.#group?.close();
      this.#group = this.#append();
    }
    const plain = new TextEncoder().encode(JSON.stringify({ sequence: this.#sequence, event }));
    this.#group.write(this.#seal(this.#group.sequence, plain));
    this.#sequence += 1;
  }

  /**
   * Send one event as a group of its own and close it: the host reads the
   * first group (SelectCodec) before it starts video.
   */
  sendAlone(event: InputEvent): void {
    this.#group?.close();
    this.#group = undefined;
    this.send(event);
    (this.#group as GroupWriter | undefined)?.close();
    this.#group = undefined;
  }

  close(): void {
    this.#group?.close();
    this.#group = undefined;
  }
}
