CREATE TABLE public."Made" (
    id integer NOT NULL,
    CONSTRAINT "Made_pkey" PRIMARY KEY (id)
);
ALTER TABLE public.t RENAME CONSTRAINT c_old TO c_new;
COMMENT ON CONSTRAINT "Made_pkey" ON public."Made" IS 'on a new table';
COMMENT ON CONSTRAINT a_pos ON public.t IS 'the owner''s';
COMMENT ON CONSTRAINT b_key ON public.t IS NULL;
