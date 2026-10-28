-- Four independently tuned complex DDCs with third-order CIC decimation by 20.
-- Synchronous active-high reset starts a new accepted-sample epoch. Configuration
-- must remain stable until reset. Input transfer is s_valid AND s_ready; gaps
-- introduce no numerical samples while accepted tokens drain the pipeline.
-- Oscillator and accepted index advance only on transfer. This index is not an
-- RF or wall-clock timestamp. An ADC-facing wrapper must report input loss if
-- its unstallable source cannot honor s_ready.
--
-- Each oscillator uses the high eight bits of its 32-bit phase accumulator.
-- Downmix=(I*cos+Q*sin, Q*cos-I*sin)>>15, retaining 18 signed bits. Three
-- 48-bit modular integrators use the current, updated preceding stage. Every
-- twentieth transfer runs three modular combs. Arithmetic >>13 normalizes the
-- CIC's gain of 8000 by 8192; the resulting gain is 0.9765625. Output is signed
-- 16-bit saturated, with a per-channel clipping flag. Raw CIC output is not a
-- complete broadcast-FM anti-alias filter; a host FIR stage follows it.
--
-- The arithmetic is registered as coefficients, products, mixed samples, three
-- integrators, three combs and saturated output. Each token uses the updated
-- preceding integrator value, preserving the same sample-domain recurrence.
-- Output valid follows its twentieth input by nine advancing clocks. A blocked
-- output freezes all pipeline tokens/state and holds data/index/clipping until
-- transfer. Reset discards every pending token. No accepted input is silently
-- discarded. This portable RTL uses no vendor FFT/DDS IP.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;

entity rf_channel_bank is
    port (
        clock              : in std_logic;
        reset              : in std_logic;
        phase_steps        : in phase_steps_t;
        s_valid            : in std_logic;
        s_ready            : out std_logic;
        s_i                : in signed(15 downto 0);
        s_q                : in signed(15 downto 0);
        m_valid            : out std_logic;
        m_ready            : in std_logic;
        m_i                : out iq_words_t;
        m_q                : out iq_words_t;
        m_clipped          : out std_logic_vector(3 downto 0);
        m_last_input_index : out unsigned(63 downto 0)
    );
end entity;

architecture rtl of rf_channel_bank is
    subtype accumulator_t is signed(47 downto 0);
    type states_t is array (0 to 3, 0 to 2) of accumulator_t;
    type products_t is array (0 to 3) of signed(31 downto 0);
    type mixed_words_t is array (0 to 3) of signed(17 downto 0);
    type indices_t is array (0 to 8) of unsigned(63 downto 0);
    signal phase : phase_steps_t := (others => (others => '0'));
    signal input_i, input_q : signed(15 downto 0) := (others => '0');
    signal sine, cosine : iq_words_t := (others => (others => '0'));
    signal product_ic, product_qs, product_qc, product_is : products_t := (others => (others => '0'));
    signal mixed_i, mixed_q : mixed_words_t := (others => (others => '0'));
    signal integrator_i, integrator_q : states_t := (others => (others => (others => '0')));
    signal comb_delay_i, comb_delay_q : states_t := (others => (others => (others => '0')));
    signal comb_result_i, comb_result_q : states_t := (others => (others => (others => '0')));
    -- Stages 0..5 carry every accepted sample; 6..8 carry only decimated groups.
    signal token_valid : std_logic_vector(0 to 8) := (others => '0');
    signal token_decimate : std_logic_vector(0 to 5) := (others => '0');
    signal token_index : indices_t := (others => (others => '0'));
    signal count : natural range 0 to 19 := 0;
    signal index : unsigned(63 downto 0) := (others => '0');
    signal advance : std_logic;
    signal valid : std_logic := '0';
    signal output_i, output_q : iq_words_t := (others => (others => '0'));
    signal clipped : std_logic_vector(3 downto 0) := (others => '0');
    signal last_index : unsigned(63 downto 0) := (others => '0');

    function saturate(value : accumulator_t) return signed is
    begin
        if value > to_signed(32767, value'length) then
            return to_signed(32767, 16);
        elsif value < to_signed(-32768, value'length) then
            return to_signed(-32768, 16);
        end if;
        return resize(value, 16);
    end function;
begin
    advance <= not valid or m_ready;
    s_ready <= advance and not reset;
    m_valid <= valid;
    m_i <= output_i;
    m_q <= output_q;
    m_clipped <= clipped;
    m_last_input_index <= last_index;

    process(clock)
        variable rom_index : natural range 0 to 255;
        variable sum_i, sum_q : signed(32 downto 0);
        variable normalized_i, normalized_q : accumulator_t;
    begin
        if rising_edge(clock) then
            if reset = '1' then
                phase <= (others => (others => '0'));
                input_i <= (others => '0'); input_q <= (others => '0');
                sine <= (others => (others => '0'));
                cosine <= (others => (others => '0'));
                product_ic <= (others => (others => '0'));
                product_qs <= (others => (others => '0'));
                product_qc <= (others => (others => '0'));
                product_is <= (others => (others => '0'));
                mixed_i <= (others => (others => '0'));
                mixed_q <= (others => (others => '0'));
                integrator_i <= (others => (others => (others => '0')));
                integrator_q <= (others => (others => (others => '0')));
                comb_delay_i <= (others => (others => (others => '0')));
                comb_delay_q <= (others => (others => (others => '0')));
                comb_result_i <= (others => (others => (others => '0')));
                comb_result_q <= (others => (others => (others => '0')));
                token_valid <= (others => '0');
                token_decimate <= (others => '0');
                token_index <= (others => (others => '0'));
                count <= 0;
                index <= (others => '0');
                valid <= '0';
                output_i <= (others => (others => '0'));
                output_q <= (others => (others => '0'));
                clipped <= (others => '0');
                last_index <= (others => '0');
            elsif advance = '1' then
                valid <= '0';
                token_valid(0) <= s_valid;
                token_valid(1 to 5) <= token_valid(0 to 4);
                token_valid(6) <= token_valid(5) and token_decimate(5);
                token_valid(7 to 8) <= token_valid(6 to 7);
                token_decimate(0) <= '0';
                token_decimate(1 to 5) <= token_decimate(0 to 4);
                for stage in 1 to 8 loop
                    token_index(stage) <= token_index(stage - 1);
                end loop;

                if s_valid = '1' then
                    input_i <= s_i; input_q <= s_q;
                    token_index(0) <= index;
                    for channel in 0 to 3 loop
                        rom_index := to_integer(phase(channel)(31 downto 24));
                        sine(channel) <= to_signed(SINE_Q15(rom_index), 16);
                        cosine(channel) <= to_signed(SINE_Q15((rom_index + 64) mod 256), 16);
                        phase(channel) <= phase(channel) + phase_steps(channel);
                    end loop;
                    if count = 19 then
                        count <= 0;
                        token_decimate(0) <= '1';
                    else
                        count <= count + 1;
                    end if;
                    index <= index + 1;
                end if;

                for channel in 0 to 3 loop
                    if token_valid(0) = '1' then
                        product_ic(channel) <= input_i * cosine(channel);
                        product_qs(channel) <= input_q * sine(channel);
                        product_qc(channel) <= input_q * cosine(channel);
                        product_is(channel) <= input_i * sine(channel);
                    end if;
                    if token_valid(1) = '1' then
                        sum_i := resize(product_ic(channel), 33) + resize(product_qs(channel), 33);
                        sum_q := resize(product_qc(channel), 33) - resize(product_is(channel), 33);
                        mixed_i(channel) <= resize(shift_right(sum_i, 15), 18);
                        mixed_q(channel) <= resize(shift_right(sum_q, 15), 18);
                    end if;
                    if token_valid(2) = '1' then
                        integrator_i(channel, 0) <= integrator_i(channel, 0) + resize(mixed_i(channel), 48);
                        integrator_q(channel, 0) <= integrator_q(channel, 0) + resize(mixed_q(channel), 48);
                    end if;
                    for stage in 1 to 2 loop
                        if token_valid(stage + 2) = '1' then
                            integrator_i(channel, stage) <= integrator_i(channel, stage)
                                + integrator_i(channel, stage - 1);
                            integrator_q(channel, stage) <= integrator_q(channel, stage)
                                + integrator_q(channel, stage - 1);
                        end if;
                    end loop;
                    if token_valid(5) = '1' and token_decimate(5) = '1' then
                        comb_result_i(channel, 0) <= integrator_i(channel, 2) - comb_delay_i(channel, 0);
                        comb_result_q(channel, 0) <= integrator_q(channel, 2) - comb_delay_q(channel, 0);
                        comb_delay_i(channel, 0) <= integrator_i(channel, 2);
                        comb_delay_q(channel, 0) <= integrator_q(channel, 2);
                    end if;
                    for stage in 1 to 2 loop
                        if token_valid(stage + 5) = '1' then
                            comb_result_i(channel, stage) <= comb_result_i(channel, stage - 1)
                                - comb_delay_i(channel, stage);
                            comb_result_q(channel, stage) <= comb_result_q(channel, stage - 1)
                                - comb_delay_q(channel, stage);
                            comb_delay_i(channel, stage) <= comb_result_i(channel, stage - 1);
                            comb_delay_q(channel, stage) <= comb_result_q(channel, stage - 1);
                        end if;
                    end loop;
                    if token_valid(8) = '1' then
                        normalized_i := shift_right(comb_result_i(channel, 2), 13);
                        normalized_q := shift_right(comb_result_q(channel, 2), 13);
                        output_i(channel) <= saturate(normalized_i);
                        output_q(channel) <= saturate(normalized_q);
                        clipped(channel) <= '0';
                        if normalized_i > to_signed(32767, 48) or normalized_i < to_signed(-32768, 48)
                            or normalized_q > to_signed(32767, 48) or normalized_q < to_signed(-32768, 48) then
                            clipped(channel) <= '1';
                        end if;
                    end if;
                end loop;
                if token_valid(8) = '1' then
                    valid <= '1';
                    last_index <= token_index(8);
                end if;
            end if;
        end if;
    end process;
end architecture;
