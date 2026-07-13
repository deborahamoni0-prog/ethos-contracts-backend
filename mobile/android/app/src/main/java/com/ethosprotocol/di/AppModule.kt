package com.ethosprotocol.di

import android.content.Context
import androidx.work.WorkManager
import com.ethosprotocol.BuildConfig
import com.ethosprotocol.api.ApiClient
import com.ethosprotocol.api.NetworkMonitor
import com.ethosprotocol.api.OfflineCache
import com.ethosprotocol.api.TokenProvider
import com.ethosprotocol.services.CheckInDatabase
import com.ethosprotocol.services.PendingCheckInDao
import dagger.Module
import dagger.Provides
import dagger.hilt.InstallIn
import dagger.hilt.android.qualifiers.ApplicationContext
import dagger.hilt.components.SingletonComponent
import javax.inject.Singleton

@Module
@InstallIn(SingletonComponent::class)
object AppModule {

    @Provides @Singleton
    fun provideApiClient(
        tokenProvider: TokenProvider,
        networkMonitor: NetworkMonitor,
        offlineCache: OfflineCache
    ): ApiClient = ApiClient(tokenProvider, networkMonitor, offlineCache, BuildConfig.API_BASE_URL)

    @Provides @Singleton
    fun provideCheckInDatabase(@ApplicationContext context: Context): CheckInDatabase =
        CheckInDatabase.create(context)

    @Provides @Singleton
    fun providePendingCheckInDao(db: CheckInDatabase): PendingCheckInDao =
        db.pendingCheckInDao()

    @Provides @Singleton
    fun provideWorkManager(@ApplicationContext context: Context): WorkManager =
        WorkManager.getInstance(context)
}
