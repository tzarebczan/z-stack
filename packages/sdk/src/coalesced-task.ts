/** One active task and one replaceable pending task. No queued snapshots. */
export function coalescedTask(): (task: () => Promise<void>) => Promise<void> {
  type Job = {
    task: () => Promise<void>;
    promise: Promise<void>;
    resolve: () => void;
    reject: (error: unknown) => void;
  };
  let active = false;
  let pending: Job | null = null;
  function run(job: Job): void {
    active = true;
    void Promise.resolve().then(() => job.task()).then(job.resolve, job.reject).finally(() => {
      if (pending) {
        const next = pending;
        pending = null;
        run(next);
      } else {
        active = false;
      }
    });
  }
  return (task) => {
    if (pending) {
      pending.task = task;
      return pending.promise;
    }
    let resolve!: Job["resolve"];
    let reject!: Job["reject"];
    const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
    const job = { task, promise, resolve, reject };
    if (active) pending = job;
    else run(job);
    return promise;
  };
}
