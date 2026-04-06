package com.matrixmedia.sdk

import com.matrixmedia.sdk.internal.networking.MMDateAdapter
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import java.util.Calendar
import java.util.TimeZone

/**
 * Unit tests for [MMDateAdapter].
 *
 * Verifies handling of the fractional ISO8601 date formats emitted by
 * the MM server (Rust) and the PTT server (Go). This was PTT bug #10:
 * Go's `time.Time.MarshalJSON` emits fractional seconds that standard
 * ISO8601 parsers reject.
 */
class MMDateAdapterTest {

    private lateinit var adapter: MMDateAdapter

    @Before
    fun setUp() {
        adapter = MMDateAdapter()
    }

    @Test
    fun `parses fractional seconds (microsecond precision)`() {
        val date = adapter.fromJson("2026-04-03T12:34:56.789012Z")
        assertNotNull(date)

        val cal = Calendar.getInstance(TimeZone.getTimeZone("UTC")).apply { time = date }
        assertEquals(2026, cal.get(Calendar.YEAR))
        assertEquals(Calendar.APRIL, cal.get(Calendar.MONTH))
        assertEquals(3, cal.get(Calendar.DAY_OF_MONTH))
        assertEquals(12, cal.get(Calendar.HOUR_OF_DAY))
        assertEquals(34, cal.get(Calendar.MINUTE))
        assertEquals(56, cal.get(Calendar.SECOND))
    }

    @Test
    fun `parses plain ISO8601 without fractional seconds`() {
        val date = adapter.fromJson("2026-04-03T12:34:56Z")
        assertNotNull(date)

        val cal = Calendar.getInstance(TimeZone.getTimeZone("UTC")).apply { time = date }
        assertEquals(2026, cal.get(Calendar.YEAR))
        assertEquals(12, cal.get(Calendar.HOUR_OF_DAY))
        assertEquals(34, cal.get(Calendar.MINUTE))
        assertEquals(56, cal.get(Calendar.SECOND))
    }

    @Test
    fun `parses millisecond precision`() {
        val date = adapter.fromJson("2026-04-03T12:34:56.789000Z")
        assertNotNull(date)
    }

    @Test
    fun `rejects invalid date string`() {
        try {
            adapter.fromJson("not-a-date")
            fail("Expected IllegalArgumentException for invalid date")
        } catch (e: IllegalArgumentException) {
            assertNotNull(e.message)
        }
    }

    @Test
    fun `rejects empty string`() {
        try {
            adapter.fromJson("")
            fail("Expected IllegalArgumentException for empty string")
        } catch (e: IllegalArgumentException) {
            assertNotNull(e.message)
        }
    }

    @Test
    fun `toJson produces valid ISO8601`() {
        val date = adapter.fromJson("2026-04-03T12:34:56Z")
        val json = adapter.toJson(date)
        assertNotNull(json)
        // Output should contain the expected components
        assert(json.contains("2026")) { "Expected year 2026 in output: $json" }
        assert(json.endsWith("Z")) { "Expected UTC 'Z' suffix in output: $json" }
    }

    @Test
    fun `round-trip preserves date`() {
        val original = "2026-04-03T12:34:56Z"
        val date = adapter.fromJson(original)
        val serialized = adapter.toJson(date)
        val reparsed = adapter.fromJson(serialized)

        val cal1 = Calendar.getInstance(TimeZone.getTimeZone("UTC")).apply { time = date }
        val cal2 = Calendar.getInstance(TimeZone.getTimeZone("UTC")).apply { time = reparsed }

        assertEquals(cal1.get(Calendar.YEAR), cal2.get(Calendar.YEAR))
        assertEquals(cal1.get(Calendar.MONTH), cal2.get(Calendar.MONTH))
        assertEquals(cal1.get(Calendar.DAY_OF_MONTH), cal2.get(Calendar.DAY_OF_MONTH))
        assertEquals(cal1.get(Calendar.HOUR_OF_DAY), cal2.get(Calendar.HOUR_OF_DAY))
        assertEquals(cal1.get(Calendar.MINUTE), cal2.get(Calendar.MINUTE))
        assertEquals(cal1.get(Calendar.SECOND), cal2.get(Calendar.SECOND))
    }
}
