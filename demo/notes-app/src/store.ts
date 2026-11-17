export interface Note {
  id: number;
  title: string;
  body: string;
  createdAt: string;
}

export interface NewNote {
  title: string;
  body?: string;
}

export interface ListOptions {
  limit?: number;
  offset?: number;
}

export interface NotePage {
  items: Note[];
  total: number;
}

export const DEFAULT_LIMIT = 20;

export class NoteStore {
  private notes: Note[] = [];
  private nextId = 1;

  create(input: NewNote): Note {
    const note: Note = {
      id: this.nextId++,
      title: input.title,
      body: input.body ?? "",
      createdAt: new Date().toISOString(),
    };
    this.notes.push(note);
    return note;
  }

  get(id: number): Note | undefined {
    return this.notes.find((note) => note.id === id);
  }

  delete(id: number): boolean {
    const index = this.notes.findIndex((note) => note.id === id);
    if (index === -1) return false;
    this.notes.splice(index, 1);
    return true;
  }

  /** Newest first. */
  list({ limit = DEFAULT_LIMIT, offset = 0 }: ListOptions = {}): NotePage {
    const newestFirst = [...this.notes].reverse();
    return {
      items: newestFirst.slice(offset, offset + limit - 1),
      total: this.notes.length,
    };
  }
}
