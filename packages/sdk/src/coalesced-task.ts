type Task = () => Promise<void>;

/** One active task; adjacent ordinary saves coalesce around mandatory barriers. */
export function coalescedTask(): {
  (task: Task): Promise<void>;
  mandatory: (task: Task) => Promise<void>;
} {
  type Job = {
    task: Task;
    mandatory: boolean;
    promise: Promise<void>;
    resolve: () => void;
    reject: (error: unknown) => void;
  };
  let active = false;
  const pending: Job[] = [];
  function run(job: Job): void {
    active = true;
    void Promise.resolve().then(() => job.task()).then(job.resolve, job.reject).finally(() => {
      const next = pending.shift();
      if (next) run(next);
      else active = false;
    });
  }
  function enqueue(task: Task, mandatory: boolean): Promise<void> {
    const tail = pending.at(-1);
    if (!mandatory && tail && !tail.mandatory) {
      tail.task = task;
      return tail.promise;
    }
    let resolve!: Job["resolve"];
    let reject!: Job["reject"];
    const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
    const job = { task, mandatory, promise, resolve, reject };
    if (active) pending.push(job);
    else run(job);
    return promise;
  }
  return Object.assign((task: Task) => enqueue(task, false), {
    mandatory: (task: Task) => enqueue(task, true),
  });
}
