/**
 * Component Task Example: Document Merger & String Processor.
 *
 * Implements the `task-runner` and `merge-task` WIT export contracts:
 *   run-task: func(input: string) -> string
 *   merge-task: func(input: string) -> string
 */

export function runTask(input: string): string {
    return "TASK_PROCESSED: " + input;
}

export function mergeTask(input: string): string {
    return "MERGED_DOCUMENT: " + input;
}
