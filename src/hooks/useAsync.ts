/**
 * One small loading hook.
 *
 * Every view in this app is "fetch a page of business data, render it", and the
 * parts that are easy to get wrong are the ones this hides: a slow response that
 * lands after the user moved on, and an error that must not leave a spinner
 * turning forever.
 */

import { useCallback, useEffect, useRef, useState } from "react";

export interface AsyncState<T> {
  readonly data: T | null;
  readonly loading: boolean;
  readonly error: string | null;
  readonly retry: () => void;
}

export function useAsync<T>(factory: () => Promise<T>, deps: readonly unknown[]): AsyncState<T> {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  // Bumped for every request; a response is only accepted while it is still the
  // newest one, so a slow page cannot overwrite the view the user moved to.
  const running = useRef(0);
  const mounted = useRef(true);
  const factoryRef = useRef(factory);
  factoryRef.current = factory;

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    const id = running.current + 1;
    running.current = id;
    setLoading(true);
    setError(null);
    factoryRef
      .current()
      .then((result) => {
        if (!mounted.current || running.current !== id) return;
        setData(result);
        setLoading(false);
      })
      .catch((rejected: unknown) => {
        if (!mounted.current || running.current !== id) return;
        setData(null);
        setError(rejected instanceof Error ? rejected.message : String(rejected));
        setLoading(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, attempt]);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);

  return { data, loading, error, retry };
}
